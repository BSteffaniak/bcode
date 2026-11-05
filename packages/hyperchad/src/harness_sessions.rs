//! Browser fallback for managed sessions. All product semantics come from `SessionView`.

use bcode_session_view::external::external_fallback_blocks;
use bcode_session_view_models::external::ExternalSessionView;

/// Render a bounded escaped fallback, readable when the execution plugin is disabled.
///
/// Buttons/forms are deliberately not invented from native tool observations: interaction dispatch
/// must bind the host-admitted source/request/generation through the managed application boundary.
/// # Errors
/// Rejects pages exceeding the shared semantic presentation budget.
pub fn external_session_html(view: &ExternalSessionView) -> Result<String, String> {
    let blocks = external_fallback_blocks(view).map_err(|error| error.to_string())?;
    let mut html = String::from("<section aria-label=\"Managed external session\">");
    for block in blocks {
        html.push_str("<pre data-external-item=\"");
        escape_into(&mut html, &block.id);
        html.push_str("\">");
        escape_into(&mut html, &block.text);
        html.push_str("</pre>");
    }
    html.push_str("</section>");
    Ok(html)
}

fn escape_into(output: &mut String, value: &str) {
    for ch in value.chars() {
        match ch {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' => output.push_str("&quot;"),
            '\'' => output.push_str("&#39;"),
            _ => output.push(ch),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_markup_and_identity_are_never_browser_code() {
        let mut escaped = String::new();
        escape_into(&mut escaped, "<script>&\"'native</script>");
        assert_eq!(
            escaped,
            "&lt;script&gt;&amp;&quot;&#39;native&lt;/script&gt;"
        );
    }
}
