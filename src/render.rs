//! Message, Markdown and code block rendering for the page.
use crate::discord::models::{ChatMessageDto, EmbedDto};
use pulldown_cmark::{html, Event, Options, Parser, Tag, TagEnd};
use serde::Serialize;
use std::{cell::RefCell, collections::VecDeque, sync::OnceLock};
use syntect::{
    easy::HighlightLines,
    highlighting::{Color, Style, Theme, ThemeItem},
    parsing::SyntaxSet,
};

#[derive(Serialize)]
pub struct Message {
    #[serde(flatten)]
    data: ChatMessageDto,
    html: String,
    rich: serde_json::Value,
}

impl From<ChatMessageDto> for Message {
    fn from(data: ChatMessageDto) -> Self {
        let html = markdown(&data.content);
        let rich = serde_json::json!({
            "embeds": data.embeds.iter().map(rendered_embed).collect::<Vec<_>>(),
            "forwarded": data.forwarded.iter().map(|message| serde_json::json!({
                "html": markdown(&message.content),
                "embeds": message.embeds.iter().map(rendered_embed).collect::<Vec<_>>()
            })).collect::<Vec<_>>()
        });
        Self { data, html, rich }
    }
}

fn rendered_embed(embed: &EmbedDto) -> serde_json::Value {
    serde_json::json!({
        "description": embed.description.as_deref().map(markdown),
        "fields": embed.fields.iter().map(|field| markdown(&field.value)).collect::<Vec<_>>()
    })
}

// Source delimiters become private markers, then independently wrapped leaves.
// Wrapping leaves keeps spoilers valid across Markdown blocks and inline tags.
fn discord_markdown(source: &str) -> (std::borrow::Cow<'_, str>, String) {
    if !source.contains("||") && !source.starts_with("-# ") && !source.contains("\n-# ") {
        return (source.into(), String::new());
    }
    let mut code = Vec::new();
    let mut text_ranges = Vec::new();
    let mut decoded = String::new();
    for (event, range) in Parser::new(source).into_offset_iter() {
        match event {
            Event::Code(_) | Event::Start(Tag::CodeBlock(_)) => code.push(range),
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                decoded.push_str(&text);
                text_ranges.push(range);
            }
            _ => {}
        }
    }
    let mut marker = "\u{e000}:".to_owned();
    while source.contains(&marker) || decoded.contains(&marker) {
        marker.push(':');
    }
    let mut replacements = Vec::new();
    let mut pending = None;
    for (offset, _) in source.match_indices("||") {
        let escaped = source[..offset]
            .bytes()
            .rev()
            .take_while(|ch| *ch == b'\\')
            .count()
            % 2
            != 0;
        if escaped
            || code.iter().any(|range| range.contains(&offset))
            || !text_ranges.iter().any(|range| range.contains(&offset))
        {
            continue;
        }
        if let Some(start) = pending.take() {
            replacements.push((start, 2, format!("{marker}0")));
            replacements.push((offset, 2, format!("{marker}1")));
        } else {
            pending = Some(offset);
        }
    }
    let mut offset = 0;
    for line in source.split_inclusive('\n') {
        if line.starts_with("-# ") && !code.iter().any(|range| range.contains(&offset)) {
            replacements.push((offset, 3, format!("{marker}2")));
            replacements.push((
                offset + line.trim_end_matches(['\n', '\r']).len(),
                0,
                format!("{marker}3"),
            ));
        }
        offset += line.len();
    }
    replacements.sort_by_key(|(offset, _, _)| *offset);
    let mut result = String::new();
    let mut copied = 0;
    for (offset, length, replacement) in replacements {
        result.push_str(&source[copied..offset]);
        result.push_str(&replacement);
        copied = offset + length;
    }
    result.push_str(&source[copied..]);
    (result.into(), marker)
}

pub fn markdown(source_text: &str) -> String {
    let (prepared, marker) = discord_markdown(source_text);
    let mut code = None;
    let mut source = String::new();
    let mut group = 0;
    let mut spoiler = None;
    let mut subtext = false;
    let mut events = Vec::new();
    let wrap = |html: String, spoiler: Option<usize>, subtext: bool, block: bool| {
        let tag = if block { "div" } else { "span" };
        let mut result = html;
        if subtext {
            result = format!("<{tag} class=\"subtext\">{result}</{tag}>");
        }
        if let Some(group) = spoiler {
            result = format!("<{tag} class=\"spoiler\" data-spoiler=\"{group}\" role=\"button\" tabindex=\"0\">{result}</{tag}>");
        }
        Event::Html(result.into())
    };
    for event in pulldown_cmark::TextMergeStream::new(Parser::new_ext(
        &prepared,
        Options::ENABLE_STRIKETHROUGH,
    )) {
        if let Event::Start(Tag::CodeBlock(kind)) = &event {
            code = Some(match kind {
                pulldown_cmark::CodeBlockKind::Fenced(language) => language
                    .split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .to_owned(),
                _ => String::new(),
            });
            source.clear();
            continue;
        }
        if let Event::End(TagEnd::CodeBlock) = event {
            events.push(wrap(
                code_block(&source, &code.take().unwrap_or_default()),
                spoiler,
                subtext,
                true,
            ));
            continue;
        }
        if code.is_some() {
            if let Event::Text(text) = event {
                source.push_str(&text);
            }
            continue;
        }
        match event {
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                if marker.is_empty() || !text.contains(&marker) {
                    if spoiler.is_none() && !subtext {
                        events.push(Event::Text(text));
                        continue;
                    }
                    let mut html = String::new();
                    html::push_html(&mut html, [Event::Text(text)].into_iter());
                    events.push(wrap(html, spoiler, subtext, false));
                    continue;
                }
                let mut remaining = text.as_ref();
                loop {
                    let (plain, next) = remaining.find(&marker).map_or((remaining, None), |at| {
                        (&remaining[..at], Some(&remaining[at + marker.len()..]))
                    });
                    if !plain.is_empty() {
                        let mut html = String::new();
                        html::push_html(&mut html, [Event::Text(plain.into())].into_iter());
                        events.push(wrap(html, spoiler, subtext, false));
                    }
                    let Some(next) = next else { break };
                    match next.as_bytes().first() {
                        Some(b'0') => {
                            group += 1;
                            spoiler = Some(group);
                        }
                        Some(b'1') => spoiler = None,
                        Some(b'2') => subtext = true,
                        Some(b'3') => subtext = false,
                        _ => {
                            events.push(Event::Text(marker.clone().into()));
                            remaining = next;
                            continue;
                        }
                    }
                    remaining = &next[1..];
                }
            }
            Event::Start(Tag::Image { .. }) | Event::End(TagEnd::Image) => {}
            Event::Start(Tag::Link {
                link_type,
                dest_url,
                title,
                id,
            }) => {
                let url = if dest_url.starts_with("https://") || dest_url.starts_with("http://") {
                    dest_url
                } else {
                    "#".into()
                };
                events.push(Event::Start(Tag::Link {
                    link_type,
                    dest_url: url,
                    title,
                    id,
                }));
            }
            leaf @ (Event::Code(_) | Event::SoftBreak | Event::HardBreak | Event::Rule) => {
                let block = matches!(leaf, Event::Rule);
                let mut html = String::new();
                html::push_html(&mut html, [leaf].into_iter());
                events.push(wrap(html, spoiler, subtext, block));
            }
            other => events.push(other),
        }
    }
    let mut html = String::new();
    html::push_html(&mut html, events.into_iter());
    html
}

fn escape(source: &str, output: &mut String) {
    for ch in source.chars() {
        match ch {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' => output.push_str("&quot;"),
            _ => output.push(ch),
        }
    }
}
fn code_block(source: &str, language: &str) -> String {
    thread_local! {
        static CACHE: RefCell<VecDeque<(String, String, String)>> = const { RefCell::new(VecDeque::new()) };
    }
    if let Some(output) = CACHE.with_borrow_mut(|cache| {
        let at = cache
            .iter()
            .position(|(text, lang, _)| text == source && lang == language)?;
        let entry = cache.remove(at)?;
        let output = entry.2.clone();
        cache.push_front(entry);
        Some(output)
    }) {
        return output;
    }
    let output = highlight(source, language);
    if source.len() <= 4096 {
        CACHE.with_borrow_mut(|cache| {
            cache.push_front((source.to_owned(), language.to_owned(), output.clone()));
            cache.truncate(32);
        });
    }
    output
}

fn highlight(source: &str, language: &str) -> String {
    static GRAMMARS: OnceLock<SyntaxSet> = OnceLock::new();
    static THEME: OnceLock<Theme> = OnceLock::new();
    let grammars = GRAMMARS.get_or_init(SyntaxSet::load_defaults_newlines);
    let theme = THEME.get_or_init(|| {
        let mut theme = Theme::default();
        theme.settings.foreground = Some(color(0xdbdee1));
        for (scope, foreground) in [
            ("comment", 0x949ba4),
            (
                "keyword, storage, support.type.property-name, meta.property-name, variable",
                0xb3baff,
            ),
            ("string", 0xa3be8c),
            ("constant.numeric, constant.language", 0xd08770),
            ("entity.name.function, support.function", 0x88c0d0),
        ] {
            theme.scopes.push(ThemeItem {
                scope: scope.parse().expect("valid syntax scope"),
                style: syntect::highlighting::StyleModifier {
                    foreground: Some(color(foreground)),
                    ..Default::default()
                },
            });
        }
        theme
    });
    let syntax = grammars
        .find_syntax_by_token(language)
        .or_else(|| grammars.find_syntax_by_first_line(source.lines().next().unwrap_or_default()))
        .unwrap_or_else(|| grammars.find_syntax_plain_text());
    let mut highlighter = HighlightLines::new(syntax, theme);
    // Chat snippets commonly contain CSS declarations without a selector block.
    if syntax.name == "CSS"
        && !source.contains('{')
        && source
            .lines()
            .any(|line| line.trim_start().starts_with("--"))
    {
        let _ = highlighter.highlight_line(":root {\n", grammars);
    }
    let mut output = String::from("<pre><code>");
    for line in source.split_inclusive('\n') {
        match highlighter.highlight_line(line, grammars) {
            Ok(spans) => {
                for (Style { foreground, .. }, text) in spans {
                    output.push_str(&format!(
                        "<span style=\"color:#{:02x}{:02x}{:02x}\">",
                        foreground.r, foreground.g, foreground.b
                    ));
                    escape(text, &mut output);
                    output.push_str("</span>");
                }
            }
            Err(_) => escape(line, &mut output),
        }
    }
    output.push_str("</code></pre>");
    output
}
fn color(rgb: u32) -> Color {
    Color {
        r: (rgb >> 16) as u8,
        g: (rgb >> 8) as u8,
        b: rgb as u8,
        a: 255,
    }
}
