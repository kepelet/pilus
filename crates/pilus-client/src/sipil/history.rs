//! A conversation's lines as a window has them: the newest page the daemon
//! sends, plus any older pages the person asked to load.
//!
//! The daemon only ever sends the newest page (or, asked, the page before a
//! line). Everything else a window shows is what it kept, so both rules here
//! keep lines instead of replacing them: a newer page must not throw away the
//! older lines already loaded, and must not leave a gap between them and itself.

use super::TranscriptItem;

/// Fold the newest page into the lines already held.
///
/// Held lines older than the page stay. That includes lines the page has since
/// moved past, which is what keeps a conversation that grew while it was open
/// without a hole between the older lines and the page. Returns the lines and
/// whether any exist before the first of them.
pub fn newest(
    held: &[TranscriptItem],
    held_more: bool,
    page: Vec<TranscriptItem>,
    page_more: bool,
) -> (Vec<TranscriptItem>, bool) {
    let Some(first) = page.first().map(|item| item.id) else {
        return (page, page_more);
    };
    let mut lines: Vec<TranscriptItem> = held
        .iter()
        .take_while(|item| item.id < first)
        .cloned()
        .collect();
    let more = if lines.is_empty() {
        page_more
    } else {
        held_more
    };
    lines.extend(page);
    (lines, more)
}

/// Put an older page in front of the lines held. `found_more` is whether the
/// daemon has lines before the page.
///
/// A daemon from before paging answers the newest page again, whatever it is
/// asked; that adds nothing, and says nothing is left, so the person is not
/// offered it again.
pub fn older(
    held: &[TranscriptItem],
    found: Vec<TranscriptItem>,
    found_more: bool,
) -> (Vec<TranscriptItem>, bool) {
    let Some(first) = held.first().map(|item| item.id) else {
        return (found, found_more);
    };
    let mut lines: Vec<TranscriptItem> = found.into_iter().filter(|item| item.id < first).collect();
    if lines.is_empty() {
        return (held.to_vec(), false);
    }
    lines.extend(held.iter().cloned());
    (lines, found_more)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sipil::ItemRole;

    fn line(id: u64) -> TranscriptItem {
        TranscriptItem {
            id,
            role: ItemRole::User,
            text: format!("m{id}"),
            tools: Vec::new(),
            calls: Vec::new(),
            call_id: None,
            is_error: false,
            reply: None,
            error: None,
            at: None,
            speaker: None,
            pane: None,
        }
    }

    fn lines(range: std::ops::RangeInclusive<u64>) -> Vec<TranscriptItem> {
        range.map(line).collect()
    }

    fn ids(items: &[TranscriptItem]) -> Vec<u64> {
        items.iter().map(|item| item.id).collect()
    }

    #[test]
    fn the_first_page_is_all_there_is_and_says_whether_more_lie_behind_it() {
        assert_eq!(newest(&[], false, lines(5..=8), true), (lines(5..=8), true));
        assert_eq!(
            newest(&[], false, lines(1..=3), false),
            (lines(1..=3), false)
        );
        assert_eq!(newest(&[], false, Vec::new(), false), (Vec::new(), false));
    }

    #[test]
    fn older_lines_are_prepended_and_the_answer_says_if_more_remain() {
        let (held, more) = older(&lines(5..=8), lines(3..=4), true);
        assert_eq!((ids(&held), more), (vec![3, 4, 5, 6, 7, 8], true));
        let (held, more) = older(&held, lines(1..=2), false);
        assert_eq!((ids(&held), more), (vec![1, 2, 3, 4, 5, 6, 7, 8], false));
    }

    #[test]
    fn a_daemon_that_ignores_paging_adds_nothing_and_is_not_asked_again() {
        let (held, more) = older(&lines(5..=8), lines(5..=8), false);
        assert_eq!((ids(&held), more), (vec![5, 6, 7, 8], false));
        // Even when it also claims more, a page with nothing older is the end.
        let (_, more) = older(&lines(5..=8), lines(6..=9), true);
        assert!(!more);
    }

    #[test]
    fn a_newer_page_keeps_the_older_lines_and_leaves_no_gap() {
        // 5..=8 was the page; 1..=4 were loaded on top; then the conversation
        // grew and the page moved on to 7..=12.
        let (held, held_more) = older(&lines(5..=8), lines(1..=4), true);
        let (now, more) = newest(&held, held_more, lines(7..=12), true);
        assert_eq!(ids(&now), (1..=12).collect::<Vec<_>>());
        assert!(more, "what lies before the first line held is still known");

        // Lines the page moved past are kept too, since dropping them would leave
        // a hole between what is held and the page.
        let (now, more) = newest(&lines(5..=8), true, lines(7..=12), true);
        assert_eq!((ids(&now), more), ((5..=12).collect::<Vec<_>>(), true));
    }

    #[test]
    fn a_page_that_starts_earlier_replaces_the_lines_it_covers() {
        // A reconnect read a longer page than before.
        let (now, more) = newest(&lines(7..=10), true, lines(3..=11), false);
        assert_eq!((ids(&now), more), ((3..=11).collect::<Vec<_>>(), false));
    }
}
