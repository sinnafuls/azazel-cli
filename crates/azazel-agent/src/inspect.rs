//! What a request is made of: its tokens by category and the text behind each category. The
//! agent keeps the last request it sent, so this is read off the request itself and never from a
//! second rendering of it.

use std::collections::HashMap;
use std::sync::Arc;

use azazel_ai::{Block, Request, Role};
use serde::{Deserialize, Serialize};

use crate::compaction::{IMAGE_TOKENS, estimate_block, estimate_text};

/// How the tools of an MCP server are named: `mcp__<server>__<tool>`.
const MCP_PREFIX: &str = "mcp__";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CategoryKind {
    /// A part of the system prompt. The category's name is the part's (`identity`, `skills`,
    /// `context_files`, `memory`, ...); parts without a name share one category with no name.
    System,
    /// Tool schemas. The category without a name holds the built-in tools; each MCP server's
    /// tools are a category named after the server.
    Tools,
    /// What was said. The category's name is `user`, `assistant`, `thinking` or `tool_calls`.
    Messages,
    /// The text of tool results.
    ToolResults,
    /// Images, wherever they are: in a prompt or in a tool result.
    Images,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Category {
    pub kind: CategoryKind,
    pub name: String,
    /// Parts, tools, blocks or images counted here.
    pub items: usize,
    /// Tokens by the counter compaction uses.
    pub estimate: u64,
    /// The estimate scaled so that the categories add up to what the provider counted; the
    /// estimate itself while the provider has reported nothing.
    pub tokens: u64,
}

/// The token breakdown of one request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextReport {
    pub agent: String,
    /// Which of the agent's requests this was, counted from 1 since the agent started.
    pub request: u32,
    /// The `provider/model` that answered; `None` until it has.
    pub model: Option<String>,
    /// The context window of the model the request was built for; 0 when unknown.
    pub window: u64,
    /// The sum of the categories' estimates.
    pub estimate: u64,
    /// Input tokens the provider counted for the request, cached ones included.
    pub reported: Option<u64>,
    /// The sum of the categories' tokens: `reported` when there is one, else `estimate`.
    pub tokens: u64,
    pub categories: Vec<Category>,
}

/// The last request an agent sent.
#[derive(Debug, Clone)]
pub struct RequestRecord {
    pub agent: String,
    /// Counted from 1 since the agent started.
    pub seq: u32,
    pub request: Arc<Request>,
    pub window: u64,
    pub model: Option<String>,
    pub reported: Option<u64>,
}

impl RequestRecord {
    pub fn report(&self) -> ContextReport {
        let groups = groups(&self.request, false);
        let estimates: Vec<u64> = groups.iter().map(|g| g.estimate).collect();
        let estimate: u64 = estimates.iter().sum();
        // A provider that counted nothing (or a request with nothing to count) leaves the
        // estimates as they are.
        let reported = self.reported.filter(|tokens| *tokens > 0 && estimate > 0);
        let shares = match reported {
            Some(total) => spread(&estimates, total),
            None => estimates,
        };
        ContextReport {
            agent: self.agent.clone(),
            request: self.seq,
            model: self.model.clone(),
            window: self.window,
            estimate,
            reported,
            tokens: shares.iter().sum(),
            categories: groups
                .into_iter()
                .zip(shares)
                .map(|(group, tokens)| Category {
                    kind: group.kind,
                    name: group.name,
                    items: group.items,
                    estimate: group.estimate,
                    tokens,
                })
                .collect(),
        }
    }

    /// The exact text of category `index` of [`RequestRecord::report`], as the request carries it.
    pub fn text(&self, index: usize) -> Option<String> {
        groups(&self.request, true)
            .into_iter()
            .nth(index)
            .map(|group| group.text)
    }
}

/// `total` shared out in proportion to `estimates`, so that the shares add up to exactly `total`.
fn spread(estimates: &[u64], total: u64) -> Vec<u64> {
    let sum: u128 = estimates.iter().map(|e| u128::from(*e)).sum();
    if sum == 0 {
        return vec![0; estimates.len()];
    }
    let mut shares: Vec<u64> = Vec::with_capacity(estimates.len());
    let mut remainders: Vec<(u128, usize)> = Vec::with_capacity(estimates.len());
    for (i, estimate) in estimates.iter().enumerate() {
        let exact = u128::from(*estimate) * u128::from(total);
        shares.push((exact / sum) as u64);
        remainders.push((exact % sum, i));
    }
    // What rounding down left over goes to the largest remainders, earlier categories first.
    let left = total - shares.iter().sum::<u64>();
    remainders.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    for (_, i) in remainders.into_iter().take(left as usize) {
        shares[i] += 1;
    }
    shares
}

struct Group {
    kind: CategoryKind,
    name: String,
    items: usize,
    estimate: u64,
    text: String,
}

struct Groups {
    list: Vec<Group>,
    with_text: bool,
}

impl Groups {
    fn at(&mut self, kind: CategoryKind, name: &str) -> usize {
        match self.list.iter().position(|g| g.kind == kind && g.name == name) {
            Some(at) => at,
            None => {
                self.list.push(Group {
                    kind,
                    name: name.to_string(),
                    items: 0,
                    estimate: 0,
                    text: String::new(),
                });
                self.list.len() - 1
            }
        }
    }

    /// Counts one item of `tokens` in a category; `piece` is its text, made only when asked for.
    fn add(&mut self, kind: CategoryKind, name: &str, tokens: u64, piece: impl FnOnce() -> String) {
        let at = self.at(kind, name);
        let with_text = self.with_text;
        let group = &mut self.list[at];
        group.items += 1;
        group.estimate += tokens;
        if with_text {
            if !group.text.is_empty() {
                group.text.push_str("\n\n");
            }
            group.text.push_str(&piece());
        }
    }
}

fn json_tokens(value: &serde_json::Value) -> u64 {
    (value.to_string().len() as u64).div_ceil(4)
}

fn pretty(value: &serde_json::Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

/// The categories of `request` in the order they are reported: the system parts as they are
/// sent, the tools (built-in first), then what was said, the tool results and the images.
fn groups(request: &Request, with_text: bool) -> Vec<Group> {
    use CategoryKind::{Images, Messages, System, ToolResults, Tools};
    let mut groups = Groups {
        list: Vec::new(),
        with_text,
    };
    // The notes that follow the conversation are instructions like the system prompt's parts.
    for part in request.system.iter().chain(&request.tail) {
        groups.add(System, &part.name, estimate_text(&part.text), || {
            part.text.clone()
        });
    }
    // The built-in tools come first even when an MCP tool is listed before them.
    if request
        .tools
        .iter()
        .any(|tool| !tool.name.starts_with(MCP_PREFIX))
    {
        groups.at(Tools, "");
    }
    for tool in &request.tools {
        let server = tool
            .name
            .strip_prefix(MCP_PREFIX)
            .map(|rest| rest.split("__").next().unwrap_or(rest))
            .unwrap_or_default();
        let tokens = estimate_text(&tool.name) + estimate_text(&tool.description) + json_tokens(&tool.schema);
        groups.add(Tools, server, tokens, || {
            format!(
                "### {}\n{}\n{}",
                tool.name,
                tool.description,
                pretty(&tool.schema)
            )
        });
    }
    for name in ["user", "assistant", "thinking", "tool_calls"] {
        groups.at(Messages, name);
    }
    groups.at(ToolResults, "");
    groups.at(Images, "");

    let mut called: HashMap<&str, &str> = HashMap::new();
    for (n, message) in request.messages.iter().enumerate() {
        let n = n + 1;
        let speaker = match message.role {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        };
        let image =
            |mime: &str, data: &str| format!("--- message {n}: {mime}, {} bytes of base64 ---", data.len());
        for block in &message.content {
            match block {
                Block::Text { text } if message.role == Role::Tool => {
                    groups.add(ToolResults, "", estimate_text(text), || {
                        format!("--- message {n} ---\n{text}")
                    });
                }
                Block::Text { text } => groups.add(Messages, speaker, estimate_text(text), || {
                    format!("--- message {n} ---\n{text}")
                }),
                Block::Thinking { text, .. } => {
                    groups.add(Messages, "thinking", estimate_text(text), || {
                        format!("--- message {n} ---\n{text}")
                    });
                }
                Block::Opaque { provider, item } => {
                    groups.add(Messages, "thinking", estimate_block(block), || {
                        format!("--- message {n}: kept for {provider} ---\n{}", pretty(item))
                    });
                }
                Block::Image { mime, data } => groups.add(Images, "", IMAGE_TOKENS, || image(mime, data)),
                Block::ToolCall { id, name, input, .. } => {
                    called.insert(id.as_str(), name.as_str());
                    groups.add(Messages, "tool_calls", estimate_block(block), || {
                        format!("--- message {n}: {name} ({id}) ---\n{}", pretty(input))
                    });
                }
                Block::ToolResult {
                    call_id,
                    content,
                    is_error,
                } => {
                    let tool = called.get(call_id.as_str()).copied().unwrap_or("a tool");
                    let failed = if *is_error { ", failed" } else { "" };
                    let mut text = String::new();
                    let mut tokens = 0;
                    for inner in content {
                        match inner {
                            Block::Text { text: piece } => {
                                tokens += estimate_text(piece);
                                text.push_str(piece);
                            }
                            Block::Image { mime, data } => {
                                groups.add(Images, "", IMAGE_TOKENS, || image(mime, data));
                            }
                            other => tokens += estimate_block(other),
                        }
                    }
                    groups.add(ToolResults, "", tokens, || {
                        format!("--- message {n}: result of {tool} ({call_id}{failed}) ---\n{text}")
                    });
                }
            }
        }
    }
    let mut list = groups.list;
    list.retain(|group| group.items > 0);
    list
}

#[cfg(test)]
mod tests {
    use azazel_ai::{Message, SystemPart, ToolSpec};
    use serde_json::json;

    use super::*;
    use crate::compaction::estimate_message;

    fn part(name: &str, text: &str) -> SystemPart {
        SystemPart {
            text: text.to_string(),
            name: name.to_string(),
            cache_boundary: false,
        }
    }

    fn tool(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.to_string(),
            description: "does a thing".to_string(),
            schema: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
            defer: false,
        }
    }

    fn assistant(content: Vec<Block>) -> Message {
        Message {
            role: Role::Assistant,
            content,
            meta: None,
        }
    }

    fn request() -> Request {
        Request {
            system: vec![
                part("identity", &"i".repeat(40)),
                part("skills", &"s".repeat(400)),
                part("", "added by a hook"),
                part("memory", &"m".repeat(80)),
            ],
            tools: vec![
                tool("mcp__github__create_issue"),
                tool("read"),
                tool("mcp__github__list_issues"),
                tool("mcp__db__query"),
                tool("edit"),
            ],
            messages: vec![
                Message::user_blocks(vec![
                    Block::text("look at src/a.rs and this picture"),
                    Block::Image {
                        mime: "image/png".into(),
                        data: "AAAA".repeat(100),
                    },
                ]),
                assistant(vec![
                    Block::Thinking {
                        text: "the file first".into(),
                        signature: None,
                        redacted: false,
                    },
                    Block::text("Reading it."),
                    Block::ToolCall {
                        id: "c1".into(),
                        name: "read".into(),
                        input: json!({"path": "src/a.rs"}),
                        signature: None,
                    },
                ]),
                Message::tool(vec![Block::ToolResult {
                    call_id: "c1".into(),
                    content: vec![
                        Block::text("fn main() {}\n".repeat(50)),
                        Block::Image {
                            mime: "image/jpeg".into(),
                            data: "B".repeat(8),
                        },
                    ],
                    is_error: false,
                }]),
                Message::user("now fix it"),
            ],
            ..Request::default()
        }
    }

    fn record(reported: Option<u64>) -> RequestRecord {
        RequestRecord {
            agent: "lead".into(),
            seq: 3,
            request: Arc::new(request()),
            window: 200_000,
            model: Some("mock/lead".into()),
            reported,
        }
    }

    fn names(report: &ContextReport) -> Vec<(CategoryKind, &str, usize)> {
        report
            .categories
            .iter()
            .map(|c| (c.kind, c.name.as_str(), c.items))
            .collect()
    }

    #[test]
    fn a_request_is_split_into_named_categories_in_a_fixed_order() {
        use CategoryKind::*;
        let report = record(None).report();
        assert_eq!(
            names(&report),
            vec![
                (System, "identity", 1),
                (System, "skills", 1),
                (System, "", 1),
                (System, "memory", 1),
                (Tools, "", 2),
                (Tools, "github", 2),
                (Tools, "db", 1),
                (Messages, "user", 2),
                (Messages, "assistant", 1),
                (Messages, "thinking", 1),
                (Messages, "tool_calls", 1),
                (ToolResults, "", 1),
                (Images, "", 2),
            ]
        );
        assert_eq!(report.categories[0].estimate, 10);
        assert_eq!(report.categories[1].estimate, 100);
        // Two images at the flat rate, whatever their size.
        assert_eq!(report.categories[12].estimate, 3000);
        // Nothing was reported, so the tokens are the estimates.
        assert!(report.categories.iter().all(|c| c.tokens == c.estimate));
        assert_eq!(report.reported, None);
        assert_eq!(report.tokens, report.estimate);
        assert_eq!((report.request, report.window), (3, 200_000));
    }

    #[test]
    fn the_message_categories_add_up_to_what_compaction_counts() {
        let request = request();
        let report = record(None).report();
        let messages: u64 = report
            .categories
            .iter()
            .filter(|c| !matches!(c.kind, CategoryKind::System | CategoryKind::Tools))
            .map(|c| c.estimate)
            .sum();
        assert_eq!(
            messages,
            request.messages.iter().map(estimate_message).sum::<u64>()
        );
        let system: u64 = report
            .categories
            .iter()
            .filter(|c| c.kind == CategoryKind::System)
            .map(|c| c.estimate)
            .sum();
        assert_eq!(
            system,
            request.system.iter().map(|p| estimate_text(&p.text)).sum::<u64>()
        );
        assert_eq!(
            report.estimate,
            report.categories.iter().map(|c| c.estimate).sum::<u64>()
        );
    }

    #[test]
    fn reported_input_tokens_rescale_every_category_and_the_shares_add_up() {
        let plain = record(None).report();
        for reported in [plain.estimate * 2, plain.estimate / 3, plain.estimate + 7, 1] {
            let report = record(Some(reported)).report();
            assert_eq!(report.reported, Some(reported));
            assert_eq!(report.tokens, reported);
            assert_eq!(report.categories.iter().map(|c| c.tokens).sum::<u64>(), reported);
            // The estimates stay what the counter said, and the order of sizes is kept.
            assert_eq!(report.estimate, plain.estimate);
            for (scaled, counted) in report.categories.iter().zip(&plain.categories) {
                assert_eq!(scaled.estimate, counted.estimate);
                let exact = counted.estimate as f64 * reported as f64 / plain.estimate as f64;
                assert!((scaled.tokens as f64 - exact).abs() < 1.0, "{scaled:?}");
            }
        }
        // A provider that counted nothing leaves the estimates alone.
        let report = record(Some(0)).report();
        assert_eq!((report.reported, report.tokens), (None, plain.estimate));
    }

    #[test]
    fn the_text_of_a_category_is_what_the_request_carries() {
        let record = record(None);
        assert_eq!(record.text(1), Some("s".repeat(400)));
        assert_eq!(record.text(2).as_deref(), Some("added by a hook"));

        let builtin = record.text(4).unwrap();
        assert!(builtin.starts_with("### read\ndoes a thing\n{"), "{builtin}");
        assert!(builtin.contains("\n\n### edit\n"));
        assert!(!builtin.contains("mcp__"));
        let github = record.text(5).unwrap();
        assert!(
            github.contains("### mcp__github__create_issue")
                && github.contains("### mcp__github__list_issues")
        );

        assert_eq!(
            record.text(7).as_deref(),
            Some("--- message 1 ---\nlook at src/a.rs and this picture\n\n--- message 4 ---\nnow fix it")
        );
        assert_eq!(
            record.text(10).as_deref(),
            Some("--- message 2: read (c1) ---\n{\n  \"path\": \"src/a.rs\"\n}")
        );
        let results = record.text(11).unwrap();
        assert!(
            results.starts_with("--- message 3: result of read (c1) ---\nfn main() {}\n"),
            "{results}"
        );
        assert_eq!(
            record.text(12).as_deref(),
            Some(
                "--- message 1: image/png, 400 bytes of base64 ---\n\n\
                 --- message 3: image/jpeg, 8 bytes of base64 ---"
            )
        );
        assert_eq!(record.text(13), None);
    }
}
