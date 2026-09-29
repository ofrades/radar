//! A small Markdown renderer for card bodies and thread messages.
//!
//! Just enough for board notes: headings, emphasis, inline code, links,
//! lists, block quotes and fenced code. It renders straight to GTK widgets so
//! a card reads properly inside the app instead of as raw text or a
//! separate viewer.

use adw::prelude::*;

/// Render Markdown text as a vertical box of widgets.
pub(super) fn render(text: &str) -> gtk::Widget {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 4);
    root.add_css_class("markdown");
    root.set_halign(gtk::Align::Fill);

    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].trim_end();
        if line.trim().is_empty() {
            i += 1;
            continue;
        }

        // Fenced code block.
        if line.trim_start().starts_with("```") {
            let mut code = String::new();
            i += 1;
            while i < lines.len() && !lines[i].trim_start().starts_with("```") {
                code.push_str(lines[i]);
                code.push('\n');
                i += 1;
            }
            i += 1;
            let label = gtk::Label::new(Some(code.trim_end_matches('\n')));
            label.set_xalign(0.0);
            label.set_wrap(true);
            label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
            label.set_selectable(true);
            label.add_css_class("md-code-block");
            root.append(&label);
            continue;
        }

        // Headings.
        let trimmed = line.trim_start();
        let hashes = trimmed.chars().take_while(|c| *c == '#').count();
        if (1..=6).contains(&hashes) && trimmed[hashes..].starts_with(' ') {
            let level = hashes.min(3);
            let label = inline_label(trimmed[hashes..].trim());
            label.add_css_class("md-heading");
            label.add_css_class(&format!("md-h{level}"));
            root.append(&label);
            i += 1;
            continue;
        }

        // Horizontal rule.
        if matches!(trimmed, "---" | "***" | "___") {
            root.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
            i += 1;
            continue;
        }

        // Block quote.
        if let Some(quote) = trimmed.strip_prefix("> ") {
            let label = inline_label(quote);
            label.add_css_class("md-quote");
            root.append(&label);
            i += 1;
            continue;
        }

        // Lists: bullets and ordered.
        if let Some(item) = bullet(trimmed) {
            root.append(&list_row("•", item));
            i += 1;
            continue;
        }
        if let Some((number, item)) = ordered(trimmed) {
            root.append(&list_row(&number, item));
            i += 1;
            continue;
        }

        // A paragraph: consecutive plain lines joined by a space.
        let mut paragraph = String::from(line.trim());
        i += 1;
        while i < lines.len() {
            let next = lines[i].trim();
            if next.is_empty() || is_block_start(next) {
                break;
            }
            paragraph.push(' ');
            paragraph.push_str(next);
            i += 1;
        }
        root.append(&inline_label(&paragraph));
    }

    root.upcast()
}

fn is_block_start(line: &str) -> bool {
    line.starts_with("```")
        || line.starts_with("# ")
        || line.starts_with("> ")
        || matches!(line, "---" | "***" | "___")
        || bullet(line).is_some()
        || ordered(line).is_some()
}

fn bullet(line: &str) -> Option<&str> {
    for marker in ["- ", "* ", "+ "] {
        if let Some(rest) = line.strip_prefix(marker) {
            return Some(rest.trim());
        }
    }
    None
}

fn ordered(line: &str) -> Option<(String, &str)> {
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let rest = &line[digits..];
    let rest = rest
        .strip_prefix(". ")
        .or_else(|| rest.strip_prefix(") "))?;
    Some((format!("{}.", &line[..digits]), rest.trim()))
}

fn list_row(marker: &str, item: &str) -> gtk::Widget {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.add_css_class("md-list-row");
    let bullet = gtk::Label::new(Some(marker));
    bullet.set_xalign(0.0);
    bullet.set_valign(gtk::Align::Start);
    bullet.add_css_class("md-bullet");
    row.append(&bullet);
    let label = inline_label(item);
    label.set_hexpand(true);
    row.append(&label);
    row.upcast()
}

fn inline_label(text: &str) -> gtk::Label {
    let label = gtk::Label::new(None);
    label.set_xalign(0.0);
    label.set_wrap(true);
    label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    label.set_selectable(true);
    label.set_markup(&inline(text));
    label.connect_activate_link(|_, uri| {
        let launcher = gtk::UriLauncher::new(uri);
        launcher.launch(None::<&gtk::Window>, None::<&gtk::gio::Cancellable>, |_| {});
        gtk::glib::Propagation::Stop
    });
    label
}

/// Turn a line of inline Markdown into Pango markup.
fn inline(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("**") {
            if let Some(end) = after.find("**") {
                out.push_str("<b>");
                out.push_str(&escape(&after[..end]));
                out.push_str("</b>");
                rest = &after[end + 2..];
                continue;
            }
        }
        if let Some(after) = rest.strip_prefix('`') {
            if let Some(end) = after.find('`') {
                out.push_str("<tt>");
                out.push_str(&escape(&after[..end]));
                out.push_str("</tt>");
                rest = &after[end + 1..];
                continue;
            }
        }
        if let Some(after) = rest.strip_prefix('[') {
            if let Some(close) = after.find("](") {
                if let Some(paren) = after[close + 2..].find(')') {
                    let label = &after[..close];
                    let url = &after[close + 2..close + 2 + paren];
                    out.push_str(&format!(
                        "<a href=\"{}\">{}</a>",
                        escape_attr(url),
                        escape(label)
                    ));
                    rest = &after[close + 2 + paren + 1..];
                    continue;
                }
            }
        }
        if let Some(after) = rest.strip_prefix('*') {
            if let Some(end) = after.find('*') {
                out.push_str("<i>");
                out.push_str(&escape(&after[..end]));
                out.push_str("</i>");
                rest = &after[end + 1..];
                continue;
            }
        }
        let ch = rest.chars().next().expect("non-empty");
        out.push_str(&escape(&ch.to_string()));
        rest = &rest[ch.len_utf8()..];
    }
    out
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(ch),
        }
    }
    out
}

fn escape_attr(text: &str) -> String {
    escape(text).replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::inline;

    #[test]
    fn inline_markup_escapes_and_styles() {
        assert_eq!(inline("a < b & c"), "a &lt; b &amp; c");
        assert_eq!(inline("**bold** and *it*"), "<b>bold</b> and <i>it</i>");
        assert_eq!(inline("use `x < y`"), "use <tt>x &lt; y</tt>");
        assert_eq!(
            inline("[docs](https://x/y)"),
            "<a href=\"https://x/y\">docs</a>"
        );
    }
}
