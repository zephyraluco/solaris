//! Getting-started tips shown in the welcome box.
//!
//! claurst keeps a few hundred tips with a persisted cooldown history; solaris only
//! needs a short rotating list, so the pick is deterministic in a session index
//! (the app passes the number of recorded sessions, which makes the tip change
//! as the app gets used).

/// One short, one-line tip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tip {
    pub content: &'static str,
}

/// Every tip, in rotation order.
pub const TIPS: &[Tip] = &[
    Tip {
        content: "Type /help for commands, or press ? for the shortcut list.",
    },
    Tip {
        content: "Tab switches between build and plan mode.",
    },
    Tip {
        content: "Ctrl+K opens the command palette.",
    },
    Tip {
        content: "Alt+Enter adds a line without sending the prompt.",
    },
    Tip {
        content: "Run /connect to pick a provider and paste an API key.",
    },
    Tip {
        content: "PageUp, PageDown and the mouse wheel scroll the transcript.",
    },
    Tip {
        content: "/model switches the model, /theme switches the palette.",
    },
    Tip {
        content: "Esc cancels whatever is open; Ctrl+C quits.",
    },
];

/// The tip for `index`, wrapping around the list.
pub fn select(index: usize) -> &'static Tip {
    if TIPS.is_empty() {
        // `TIPS` is a constant with entries; this keeps `select` total anyway.
        return &Tip {
            content: "Type /help for commands.",
        };
    }
    &TIPS[index % TIPS.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tips_are_non_empty_one_liners() {
        assert!(TIPS.len() >= 5, "the rotation would be too obvious");
        for tip in TIPS {
            assert!(!tip.content.trim().is_empty());
            assert!(!tip.content.contains('\n'), "{:?}", tip.content);
        }
    }

    #[test]
    fn selecting_wraps_around_the_list() {
        assert_eq!(select(0).content, TIPS[0].content);
        assert_eq!(select(TIPS.len()).content, TIPS[0].content);
        assert_eq!(select(TIPS.len() + 3).content, TIPS[3].content);
    }
}
