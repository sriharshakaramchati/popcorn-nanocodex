//! The model-facing `browser` tool backed by the rented session.
//!
//! The tool drives the remote browser over CDP through `chromiumoxide`. The
//! CDP URL is constructed and held here, in host code; tool inputs and outputs
//! carry page state only, so session secrets never enter the model's context.

use std::sync::Arc;

use async_trait::async_trait;
use chromiumoxide::Page;
use nanocodex_oai_api::responses::JsonSchema;
use nanocodex_oai_api::tools::{Tool, ToolContext, ToolDefinition, ToolInput, ToolOutput, ToolResult};
use serde::Deserialize;
use serde_json::json;

/// How much page text one `read` returns to the model.
const READ_LIMIT: usize = 8_000;

/// A Nanocodex tool that drives the rented Popcorn browser session.
pub struct PopcornBrowserTool {
    browser: Arc<chromiumoxide::Browser>,
    page: Arc<tokio::sync::Mutex<Option<Page>>>,
}

impl std::fmt::Debug for PopcornBrowserTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PopcornBrowserTool").finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct BrowserInput {
    action: String,
    url: Option<String>,
    selector: Option<String>,
    text: Option<String>,
    js: Option<String>,
}

impl PopcornBrowserTool {
    pub(crate) fn new(browser: Arc<chromiumoxide::Browser>) -> Self {
        Self {
            browser,
            page: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }

    /// Returns the working page, opening one on first use.
    async fn page(&self) -> Result<Page, chromiumoxide::error::CdpError> {
        let mut guard = self.page.lock().await;
        if let Some(page) = guard.as_ref() {
            return Ok(page.clone());
        }
        let page = self.browser.new_page("about:blank").await?;
        *guard = Some(page.clone());
        Ok(page)
    }
}

#[async_trait]
impl Tool for PopcornBrowserTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(
            "browser",
            "Drive the remote Popcorn browser session. Actions: `navigate` to a URL, \
             `read` the current page's text, `click` a CSS selector, `type` text into a \
             CSS selector, or `evaluate` a JavaScript expression. The browser runs in an \
             isolated remote session; a human may be watching or logged in.",
            JsonSchema::from(json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["navigate", "read", "click", "type", "evaluate"],
                        "description": "What to do in the browser."
                    },
                    "url": {
                        "type": "string",
                        "description": "Destination URL, required for `navigate`."
                    },
                    "selector": {
                        "type": "string",
                        "description": "CSS selector, required for `click` and `type`."
                    },
                    "text": {
                        "type": "string",
                        "description": "Text to enter, required for `type`."
                    },
                    "js": {
                        "type": "string",
                        "description": "JavaScript expression, required for `evaluate`."
                    }
                },
                "required": ["action"],
                "additionalProperties": false
            })),
        )
    }

    async fn execute(&self, input: ToolInput, _context: ToolContext<'_>) -> ToolResult {
        let input: BrowserInput = input.decode_json()?;
        let page = self.page().await?;
        match input.action.as_str() {
            "navigate" => {
                let url = input
                    .url
                    .ok_or("`navigate` requires `url`")?;
                page.goto(&url).await?;
                let title: serde_json::Value = page
                    .evaluate("document.title")
                    .await?
                    .value()
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                Ok(ToolOutput::text(format!(
                    "navigated to {url}; title: {}",
                    title.as_str().unwrap_or_default()
                )))
            }
            "read" => {
                let script = format!(
                    "JSON.stringify({{title: document.title, url: location.href, text: (document.body ? document.body.innerText : '').slice(0, {READ_LIMIT})}})"
                );
                let value = page.evaluate(script).await?;
                let value = value.value().cloned().unwrap_or(serde_json::Value::Null);
                let rendered = match value.as_str() {
                    Some(encoded) => serde_json::from_str::<serde_json::Value>(encoded)
                        .map(|parsed| {
                            format!(
                                "title: {}\nurl: {}\n\n{}",
                                parsed["title"].as_str().unwrap_or_default(),
                                parsed["url"].as_str().unwrap_or_default(),
                                parsed["text"].as_str().unwrap_or_default()
                            )
                        })
                        .unwrap_or_else(|_| encoded.to_owned()),
                    None => value.to_string(),
                };
                Ok(ToolOutput::text(rendered))
            }
            "click" => {
                let selector = input
                    .selector
                    .ok_or("`click` requires `selector`")?;
                page.find_element(&selector).await?.click().await?;
                Ok(ToolOutput::text(format!("clicked `{selector}`")))
            }
            "type" => {
                let selector = input
                    .selector
                    .ok_or("`type` requires `selector`")?;
                let text = input.text.ok_or("`type` requires `text`")?;
                let element = page.find_element(&selector).await?;
                element.click().await?;
                element.type_str(&text).await?;
                Ok(ToolOutput::text(format!(
                    "typed {} characters into `{selector}`",
                    text.chars().count()
                )))
            }
            "evaluate" => {
                let js = input.js.ok_or("`evaluate` requires `js`")?;
                let value = page.evaluate(js).await?;
                let value = value.value().cloned().unwrap_or(serde_json::Value::Null);
                Ok(ToolOutput::text(value.to_string()))
            }
            other => Err(format!(
                "unknown browser action `{other}`; use navigate, read, click, type, or evaluate"
            )
            .into()),
        }
    }
}
