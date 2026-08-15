//! Recognising the approval requests that agents send as ordinary chat.
//!
//! Humans and agents share these rooms, but the protocol has no event kind for
//! "may I?", so an agent asks in prose and states the literal tokens it will
//! accept in reply. Two conventions appear in practice: a dangerous command
//! gated behind `/approve` and `/deny`, and a narrower question answered with
//! the index of a numbered choice.
//!
//! Detection is deliberately strict. A message counts as a request only when it
//! also spells out how to answer, so a colleague writing "I approve" is never
//! mistaken for a control that buzztui should answer on your behalf.

/// What an agent is waiting on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// A dangerous operation gated behind the `/approve` and `/deny` tokens.
    Command {
        /// The operation as the agent described it, when it arrived fenced.
        command: Option<String>,
        /// Why the agent decided the operation needed asking about.
        reason: Option<String>,
    },
    /// A numbered menu, answered with the index of one choice.
    Question {
        question: String,
        options: Vec<String>,
    },
}

/// One answer the modal can offer, paired with the text it sends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub label: String,
    /// Exactly what is published back to the room.
    pub reply: String,
    pub kind: ChoiceKind,
}

/// How far an answer reaches beyond the request in front of you.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChoiceKind {
    /// Answer this request and nothing else.
    Once,
    /// Answer this one and let later command requests from the same agent
    /// through unattended for the rest of the session.
    Always,
    /// Refuse.
    Deny,
}

impl Request {
    /// Reads a request out of a chat message, or decides there isn't one.
    pub fn parse(content: &str) -> Option<Self> {
        parse_command(content).or_else(|| parse_question(content))
    }

    /// The answers to offer, in the order they should be listed. `agent` names
    /// the requester so a standing grant says whom it is granted to.
    pub fn choices(&self, agent: &str) -> Vec<Choice> {
        match self {
            Request::Command { .. } => vec![
                Choice {
                    label: "allow once".to_string(),
                    reply: "/approve".to_string(),
                    kind: ChoiceKind::Once,
                },
                Choice {
                    label: format!("allow always for {agent}"),
                    reply: "/approve".to_string(),
                    kind: ChoiceKind::Always,
                },
                Choice {
                    label: "deny".to_string(),
                    reply: "/deny".to_string(),
                    kind: ChoiceKind::Deny,
                },
            ],
            // A menu is a genuine choice between alternatives, so there is no
            // honest way to grant it in advance: every option stays explicit.
            Request::Question { options, .. } => {
                let mut choices: Vec<Choice> = options
                    .iter()
                    .enumerate()
                    .map(|(index, option)| Choice {
                        label: option.clone(),
                        reply: (index + 1).to_string(),
                        kind: ChoiceKind::Once,
                    })
                    .collect();
                choices.push(Choice {
                    label: "deny".to_string(),
                    reply: "deny".to_string(),
                    kind: ChoiceKind::Deny,
                });
                choices
            }
        }
    }

    /// A one-line summary for toasts and for the modal's heading.
    pub fn summary(&self) -> &str {
        match self {
            Request::Command { command, .. } => command
                .as_deref()
                .map(str::trim)
                .filter(|command| !command.is_empty())
                .unwrap_or("an operation it will not name"),
            Request::Question { question, .. } => question,
        }
    }

    /// Whether a standing session grant may answer this without asking.
    pub fn grantable(&self) -> bool {
        matches!(self, Request::Command { .. })
    }
}

/// The `/approve`-and-`/deny` convention. Both tokens must appear quoted, which
/// is what distinguishes an instruction from someone discussing approval.
fn parse_command(content: &str) -> Option<Request> {
    if !content.contains("`/approve`") || !content.contains("`/deny`") {
        return None;
    }
    Some(Request::Command {
        command: fenced_block(content),
        reason: labelled_paragraph(content, "Reason:"),
    })
}

/// The numbered-menu convention, which states how to answer in the same breath.
fn parse_question(content: &str) -> Option<Request> {
    let lowered = content.to_lowercase();
    if !lowered.contains("reply with the number") {
        return None;
    }

    let mut question = String::new();
    let mut options = Vec::new();
    for line in content.lines() {
        match numbered_option(line) {
            Some(option) => options.push(option),
            None if options.is_empty() => {
                let line = line.trim().trim_start_matches(['❓', '⚠', '❔']).trim();
                if !line.is_empty() {
                    if !question.is_empty() {
                        question.push(' ');
                    }
                    question.push_str(line);
                }
            }
            // Everything past the menu is the instruction on how to answer,
            // which the modal replaces rather than repeats.
            None => {}
        }
    }

    if options.is_empty() {
        return None;
    }
    Some(Request::Question { question, options })
}

/// `  1. Approve the write` becomes `Approve the write`.
fn numbered_option(line: &str) -> Option<String> {
    let trimmed = line.trim();
    let digits: String = trimmed.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    let rest = trimmed[digits.len()..].trim_start();
    let rest = rest.strip_prefix('.').or_else(|| rest.strip_prefix(')'))?;
    let option = rest.trim();
    (!option.is_empty()).then(|| option.to_string())
}

/// The contents of the first fenced block, which is where these agents put the
/// operation they want to run.
fn fenced_block(content: &str) -> Option<String> {
    let mut lines = content.lines().skip_while(|line| !is_fence(line));
    lines.next()?;
    let body: Vec<&str> = lines.take_while(|line| !is_fence(line)).collect();
    let body = body.join("\n").trim().to_string();
    (!body.is_empty()).then_some(body)
}

fn is_fence(line: &str) -> bool {
    line.trim_start().starts_with("```")
}

/// The paragraph introduced by `label`, gathered up to the next blank line so a
/// wrapped reason survives intact.
fn labelled_paragraph(content: &str, label: &str) -> Option<String> {
    let mut lines = content.lines().skip_while(|line| !line.starts_with(label));
    let first = lines.next()?[label.len()..].trim().to_string();
    let mut paragraph = first;
    for line in lines.take_while(|line| !line.trim().is_empty()) {
        paragraph.push(' ');
        paragraph.push_str(line.trim());
    }
    let paragraph = paragraph.trim().to_string();
    (!paragraph.is_empty()).then_some(paragraph)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verbatim from a Hermes request, fenced block and all.
    const DANGEROUS: &str = "⚠️ **Dangerous command requires approval:**\n\
        ```\n\
        <write to AGENTS.md>\n\
        ```\n\
        Reason: Write to protected agent-instruction file(s): AGENTS.md. These files steer future agent behavior; approval is always required (not bypassed by auto-approve).\n\
        \n\
        Reply `/approve` to execute this one operation, or `/deny` to cancel.";

    /// Verbatim from the menu Hermes fell back to when `/approve` was swallowed.
    const MENU: &str = "❓ The protected-file guard still did not recognize the approval in your message. Please approve through this confirmation control so I can write AGENTS.md and push the repository.\n\
        \n\
        \x20 1. Approve AGENTS.md write and git push\n\
        \n\
        Reply with the number, the option text, or your own answer.";

    #[test]
    fn a_dangerous_command_carries_its_operation_and_reason() {
        let Some(Request::Command { command, reason }) = Request::parse(DANGEROUS) else {
            panic!("the dangerous-command convention was not recognised");
        };
        assert_eq!(command.as_deref(), Some("<write to AGENTS.md>"));
        let reason = reason.expect("the reason line is part of the request");
        assert!(reason.starts_with("Write to protected agent-instruction file(s)"));
        assert!(reason.ends_with("(not bypassed by auto-approve)."));
    }

    #[test]
    fn a_dangerous_command_offers_once_always_and_deny() {
        let request = Request::parse(DANGEROUS).expect("recognised");
        let choices = request.choices("Hermes");
        assert_eq!(
            choices
                .iter()
                .map(|choice| (choice.kind, choice.reply.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (ChoiceKind::Once, "/approve"),
                (ChoiceKind::Always, "/approve"),
                (ChoiceKind::Deny, "/deny"),
            ]
        );
        assert_eq!(choices[1].label, "allow always for Hermes");
        assert!(request.grantable());
    }

    #[test]
    fn a_menu_is_answered_by_index_and_cannot_be_granted_in_advance() {
        let Some(request @ Request::Question { .. }) = Request::parse(MENU) else {
            panic!("the numbered-menu convention was not recognised");
        };
        let Request::Question { question, options } = &request else {
            unreachable!()
        };
        assert!(question.starts_with("The protected-file guard still did not recognize"));
        assert_eq!(
            options,
            &vec!["Approve AGENTS.md write and git push".to_string()]
        );

        let choices = request.choices("Hermes");
        assert_eq!(choices[0].reply, "1");
        assert_eq!(choices[0].label, "Approve AGENTS.md write and git push");
        assert_eq!(choices.last().unwrap().kind, ChoiceKind::Deny);
        assert!(
            !request.grantable(),
            "a menu must keep every answer explicit"
        );
    }

    #[test]
    fn ordinary_conversation_is_not_a_request() {
        for innocent in [
            "I approve, and push it to git",
            "@Hermes /approve",
            "No pending command to approve.",
            "we should deny that by default",
            "reply with the number of the ticket when you find it",
            "```\n/approve\n```",
        ] {
            assert_eq!(
                Request::parse(innocent),
                None,
                "{innocent:?} must not be treated as an approval request"
            );
        }
    }

    #[test]
    fn a_request_without_a_fenced_operation_still_parses() {
        let content = "Approval needed. Reply `/approve` to continue, or `/deny` to stop.";
        let Some(Request::Command { command, reason }) = Request::parse(content) else {
            panic!("recognised");
        };
        assert_eq!(command, None);
        assert_eq!(reason, None);
        assert_eq!(
            Request::parse(content).unwrap().summary(),
            "an operation it will not name"
        );
    }

    #[test]
    fn a_menu_accepts_parenthesised_indices() {
        let content = "❓ pick one\n\n1) first\n2) second\n\nReply with the number.";
        let Some(Request::Question { options, .. }) = Request::parse(content) else {
            panic!("recognised");
        };
        assert_eq!(options, vec!["first".to_string(), "second".to_string()]);
    }
}
