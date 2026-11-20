//! Glob-line filter rules, used for download allow/blocklists (`action`
//! applies) and chapter-title auto-skip (always skip; see
//! `should_skip_chapter`).
//!
//! Stored as JSON in `subscriptions.download_filter_rules` and
//! `chapter_skip_rules`. A plain keyword matches as a case-insensitive
//! substring ("bonus" matches "This is a Bonus Episode!", compiled to
//! `*bonus*`). A pattern containing `*`, `?` or `[...]` is used as written,
//! so "AMA*" means "starts with AMA".

use globset::{Glob, GlobBuilder, GlobMatcher};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchTarget {
    Title,
    Description,
    Both,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Allow,
    Block,
}

/// A rule as stored in the JSON columns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FilterRule {
    pub pattern: String,
    pub target: MatchTarget,
    pub action: Action,
}

#[derive(Debug, thiserror::Error)]
pub enum FilterError {
    #[error("invalid pattern {pattern:?}: {source}")]
    InvalidPattern {
        pattern: String,
        #[source]
        source: globset::Error,
    },
}

/// A rule with its glob compiled. Compile a rule set once per pass
/// (`compile_rules`) and reuse it for every episode.
#[derive(Debug)]
pub struct CompiledFilterRule {
    matcher: GlobMatcher,
    target: MatchTarget,
    action: Action,
}

impl FilterRule {
    pub fn compile(&self) -> Result<CompiledFilterRule, FilterError> {
        let effective_pattern = if has_glob_metacharacters(&self.pattern) {
            self.pattern.clone()
        } else {
            format!("*{}*", self.pattern)
        };
        let glob: Glob = GlobBuilder::new(&effective_pattern)
            .case_insensitive(true)
            // Lets `*` cross what globset treats as a path separator; these
            // are titles and show notes, not paths.
            .literal_separator(false)
            .build()
            .map_err(|source| FilterError::InvalidPattern {
                pattern: self.pattern.clone(),
                source,
            })?;
        Ok(CompiledFilterRule {
            matcher: glob.compile_matcher(),
            target: self.target,
            action: self.action,
        })
    }
}

fn has_glob_metacharacters(pattern: &str) -> bool {
    pattern.contains(['*', '?', '[', ']'])
}

/// One invalid pattern fails the whole set. The filter editor validates on
/// entry too.
pub fn compile_rules(rules: &[FilterRule]) -> Result<Vec<CompiledFilterRule>, FilterError> {
    rules.iter().map(FilterRule::compile).collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterDecision {
    Allow,
    Block,
}

/// Rules are checked in order and the first match decides, so order matters:
/// "Allow *June*" above "Block Ju*" lets June episodes through, and the
/// reverse order blocks them. An episode matching no rule is allowed.
///
/// Only the automatic download pipeline applies filters; manual queueing and
/// downloading bypass them.
pub fn evaluate(rules: &[CompiledFilterRule], title: &str, show_notes_text: &str) -> FilterDecision {
    for rule in rules {
        let is_match = match rule.target {
            MatchTarget::Title => rule.matcher.is_match(title),
            MatchTarget::Description => rule.matcher.is_match(show_notes_text),
            MatchTarget::Both => rule.matcher.is_match(title) || rule.matcher.is_match(show_notes_text),
        };
        if is_match {
            return match rule.action {
                Action::Block => FilterDecision::Block,
                Action::Allow => FilterDecision::Allow,
            };
        }
    }
    FilterDecision::Allow
}

/// Evaluated at playback rather than cached on `chapters`, so editing a rule
/// applies to chapters already fetched (see schema.sql).
pub fn should_skip_chapter(rules: &[CompiledFilterRule], chapter_title: &str) -> bool {
    rules.iter().any(|r| r.matcher.is_match(chapter_title))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(pattern: &str, target: MatchTarget, action: Action) -> FilterRule {
        FilterRule { pattern: pattern.to_string(), target, action }
    }

    #[test]
    fn empty_rules_allow_everything() {
        let compiled = compile_rules(&[]).unwrap();
        assert_eq!(evaluate(&compiled, "any title", "any notes"), FilterDecision::Allow);
    }

    #[test]
    fn plain_keyword_matches_as_case_insensitive_substring() {
        let rules = vec![rule("bonus", MatchTarget::Title, Action::Block)];
        let compiled = compile_rules(&rules).unwrap();
        assert_eq!(
            evaluate(&compiled, "This is a BONUS episode!", ""),
            FilterDecision::Block
        );
        assert_eq!(evaluate(&compiled, "A regular episode", ""), FilterDecision::Allow);
    }

    #[test]
    fn first_matching_rule_wins_regardless_of_action() {
        // A block rule above a matching allow rule wins; below it, it
        // doesn't.
        let rules = vec![
            rule("ad read", MatchTarget::Description, Action::Block), // Checked first.
            rule("*", MatchTarget::Title, Action::Allow), // Reached otherwise.
        ];
        let compiled = compile_rules(&rules).unwrap();
        assert_eq!(
            evaluate(&compiled, "Episode 42", "contains an Ad Read segment"),
            FilterDecision::Block
        );
        assert_eq!(evaluate(&compiled, "Episode 43", "clean episode"), FilterDecision::Allow);
    }

    #[test]
    fn rule_order_can_carve_out_an_exception_to_a_broader_rule() {
        // An Allow rule above a broader Block rule lets its matches through.
        let rules = vec![
            rule("*June*", MatchTarget::Title, Action::Allow),
            rule("Ju*", MatchTarget::Title, Action::Block),
        ];
        let compiled = compile_rules(&rules).unwrap();
        assert_eq!(evaluate(&compiled, "June Special Episode", ""), FilterDecision::Allow);
        // No exception applies, so the block rule does.
        assert_eq!(evaluate(&compiled, "July Roundup", ""), FilterDecision::Block);

        // Reversed, the block rule is checked first and "June" starts with
        // "Ju".
        let reversed = vec![
            rule("Ju*", MatchTarget::Title, Action::Block),
            rule("*June*", MatchTarget::Title, Action::Allow),
        ];
        let compiled_reversed = compile_rules(&reversed).unwrap();
        assert_eq!(evaluate(&compiled_reversed, "June Special Episode", ""), FilterDecision::Block);
    }

    #[test]
    fn explicit_glob_pattern_is_anchored_not_substring() {
        let rules = vec![rule("AMA*", MatchTarget::Title, Action::Block)];
        let compiled = compile_rules(&rules).unwrap();
        assert_eq!(evaluate(&compiled, "AMA Episode", ""), FilterDecision::Block);
        // An explicit glob anchors the match, unlike the substring behavior.
        assert_eq!(evaluate(&compiled, "Not an AMA at all", ""), FilterDecision::Allow);
    }

    #[test]
    fn both_target_checks_title_and_description() {
        let rules = vec![rule("sponsor", MatchTarget::Both, Action::Block)];
        let compiled = compile_rules(&rules).unwrap();
        assert_eq!(evaluate(&compiled, "Sponsored Episode", "clean"), FilterDecision::Block);
        assert_eq!(evaluate(&compiled, "Clean Title", "has a sponsor mention"), FilterDecision::Block);
        assert_eq!(evaluate(&compiled, "Clean Title", "clean notes"), FilterDecision::Allow);
    }

    #[test]
    fn chapter_skip_matches_any_rule() {
        let rules = vec![
            rule("Sponsored Segment", MatchTarget::Title, Action::Block),
            rule("Ad Read", MatchTarget::Title, Action::Block),
        ];
        let compiled = compile_rules(&rules).unwrap();
        assert!(should_skip_chapter(&compiled, "Ad Read"));
        assert!(!should_skip_chapter(&compiled, "Interview with the host"));
    }

    #[test]
    fn invalid_pattern_reports_the_offending_pattern() {
        let rules = vec![rule("[unterminated", MatchTarget::Title, Action::Block)];
        let err = compile_rules(&rules).unwrap_err();
        assert!(matches!(err, FilterError::InvalidPattern { pattern, .. } if pattern == "[unterminated"));
    }
}
