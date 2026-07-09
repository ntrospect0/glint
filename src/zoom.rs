// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! App-level zoom target — names the widget currently filling the
//! centered zoom overlay.
//!
//! `parent_id` is always a top-level [`WidgetManager`] key. `child_id`
//! is `Some` only for widgets that live inside a stack (the child-isolated
//! render path mandated by CEO Q1): in that case the zoom frame renders the
//! named child widget alone, bypassing the stack's tab-strip chrome. For
//! every leaf widget `child_id` is `None`.
//!
//! [`WidgetManager`]: crate::widgets::WidgetManager

/// Names the zoom target. Carried in `App::zoom_target` while zoom is
/// active; `None` means zoom is off.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZoomTarget {
    /// Top-level key in `WidgetManager`. Always present and always a
    /// top-level manager entry — never a child's own id.
    pub parent_id: String,
    /// `Some(child_id)` only for stack-resident widgets (CEO Q1:
    /// child-isolated rendering). `None` for all leaf widgets.
    pub child_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zoom_target_leaf() {
        let a = ZoomTarget {
            parent_id: "clock".into(),
            child_id: None,
        };
        let b = a.clone();
        assert_eq!(a, b);
    }

    #[test]
    fn zoom_target_stack_child() {
        let leaf = ZoomTarget {
            parent_id: "my_stack".into(),
            child_id: None,
        };
        let child = ZoomTarget {
            parent_id: "my_stack".into(),
            child_id: Some("news".into()),
        };
        assert_ne!(leaf, child);
    }

    #[test]
    fn zoom_target_eq_both_none_child() {
        let a = ZoomTarget {
            parent_id: "weather".into(),
            child_id: None,
        };
        let b = ZoomTarget {
            parent_id: "weather".into(),
            child_id: None,
        };
        assert_eq!(a, b);
    }
}
