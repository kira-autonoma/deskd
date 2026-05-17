//! Topic glob matching for federation (#463).
//!
//! Two wildcards:
//! - `*` matches exactly **one** segment (split by `.`).
//!   `voice.*` matches `voice.recorded` but **not** `voice.recorded.partial`.
//! - `>` matches **one or more** trailing segments. Must be the final element
//!   in the pattern. `phone.>` matches `phone.voice.recorded`,
//!   `phone.voice.recorded.partial`, etc.
//!
//! Patterns without wildcards match exactly. Empty patterns and empty topics
//! never match. The match function is allocation-free in the hot path.
//!
//! This is a transport-layer routing primitive — separate from the existing
//! local-bus glob in `bus_server` (which uses trailing-`*` prefix matching).
//! Federation segments are `.`-delimited (`<device>.<domain>.<event>` per the
//! epic's namespace convention).

/// Return `true` if `topic` matches `pattern` under federation glob rules.
///
/// See the module docs for the wildcard semantics.
pub fn matches(pattern: &str, topic: &str) -> bool {
    if pattern.is_empty() || topic.is_empty() {
        return false;
    }
    // Fast path: literal match (no wildcards present).
    if !pattern.contains('*') && !pattern.contains('>') {
        return pattern == topic;
    }

    let pat_segs: Vec<&str> = pattern.split('.').collect();
    let topic_segs: Vec<&str> = topic.split('.').collect();

    for (i, pseg) in pat_segs.iter().enumerate() {
        match *pseg {
            ">" => {
                // Must be the last segment in the pattern, and there must be
                // at least one remaining topic segment.
                if i + 1 != pat_segs.len() {
                    // Malformed pattern: `>` not at end → no match.
                    return false;
                }
                return topic_segs.len() > i;
            }
            "*" => {
                if i >= topic_segs.len() {
                    return false;
                }
                // any single segment matches; continue.
            }
            literal => {
                if i >= topic_segs.len() || topic_segs[i] != literal {
                    return false;
                }
            }
        }
    }
    // Both consumed fully — equal segment count, all matched.
    pat_segs.len() == topic_segs.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_match() {
        assert!(matches("voice.recorded", "voice.recorded"));
        assert!(!matches("voice.recorded", "voice.recorded.partial"));
        assert!(!matches("voice.recorded", "voice"));
        assert!(!matches("voice.recorded", "voice.published"));
    }

    #[test]
    fn star_matches_one_segment() {
        assert!(matches("voice.*", "voice.recorded"));
        assert!(matches("voice.*", "voice.published"));
        // > one segment is forbidden by `*`.
        assert!(!matches("voice.*", "voice.recorded.partial"));
        // zero segments doesn't match either.
        assert!(!matches("voice.*", "voice"));
    }

    #[test]
    fn star_in_middle() {
        assert!(matches("a.*.c", "a.b.c"));
        assert!(matches("a.*.c", "a.x.c"));
        assert!(!matches("a.*.c", "a.b.d"));
        assert!(!matches("a.*.c", "a.b.c.d"));
    }

    #[test]
    fn gt_matches_one_or_more_trailing_segments() {
        assert!(matches("phone.>", "phone.voice"));
        assert!(matches("phone.>", "phone.voice.recorded"));
        assert!(matches("phone.>", "phone.voice.recorded.partial"));
        // exactly the prefix with nothing after must NOT match (>= 1 segment).
        assert!(!matches("phone.>", "phone"));
    }

    #[test]
    fn gt_only_valid_at_end() {
        // Malformed pattern: > not at end. Treated as no match.
        assert!(!matches("phone.>.recorded", "phone.voice.recorded"));
    }

    #[test]
    fn empty_inputs_never_match() {
        assert!(!matches("", "anything"));
        assert!(!matches("voice.*", ""));
        assert!(!matches("", ""));
    }

    #[test]
    fn star_does_not_cross_segment_boundary() {
        // The local-bus glob (`telegram:*` matching `telegram:foo:bar`) is
        // separate; federation `*` is strictly single-segment.
        assert!(!matches("a.*", "a.b.c"));
    }

    #[test]
    fn table_driven_cases() {
        let cases: &[(&str, &str, bool)] = &[
            ("voice.*", "voice.recorded", true),
            ("voice.*", "voice.recorded.partial", false),
            ("phone.>", "phone.voice.recorded", true),
            ("phone.>", "phone", false),
            ("agent.dev.completed", "agent.dev.completed", true),
            ("agent.dev.completed", "agent.dev", false),
            ("agent.dev.completed", "agent.dev.completed.extra", false),
            ("*.voice.recorded", "phone.voice.recorded", true),
            ("*.voice.recorded", "mac.voice.recorded", true),
            ("*.voice.recorded", "phone.cursor.recorded", false),
            ("*.*", "a.b", true),
            ("*.*", "a", false),
            (">", "phone.voice", true),
            (">", "phone", true),
        ];
        for (pat, topic, expected) in cases {
            assert_eq!(
                matches(pat, topic),
                *expected,
                "pattern={pat} topic={topic}"
            );
        }
    }
}
