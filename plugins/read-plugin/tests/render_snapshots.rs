use bcode_markdown_render::{MarkdownRenderOptions, render_markdown_lines};

fn render(markdown: &str, width: u16) -> String {
    render_markdown_lines(markdown, MarkdownRenderOptions::new(width))
        .iter()
        .enumerate()
        .map(|(index, line)| {
            let content = line
                .spans
                .iter()
                .map(|span| span.content.as_str())
                .collect::<String>();
            format!("{index:02} │ {content}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn representative_markdown_at_terminal_width() {
    insta::assert_snapshot!(render(
        r#"# Heading

Paragraph with **bold**, *emphasis*, ~~strike~~, [link](https://example.com), `inline code`, and Unicode 🦀 café.

> [!NOTE]
> Blockquote alert.

1. Ordered
2. List

- Unordered
- [x] Done
- [ ] Pending

| Wide heading | Other heading |
| --- | --- |
| value | another value |

```rust
fn main() {
    println!("hello");
}
```

[malformed](<"#,
        80,
    ));
}

#[test]
fn representative_markdown_at_narrow_width() {
    insta::assert_snapshot!(render(
        r#"# Narrow

A paragraph with **strong text**, *emphasis*, [a link](https://example.com), `inline code`, and Unicode 🦀 café.

> quoted words wrap safely

- [x] complete
- [ ] pending

| Column one | Column two |
| --- | --- |
| a long value | another long value |

```rust
let answer = 42;
```"#,
        24,
    ));
}
