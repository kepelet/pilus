//! The Home screen's in-window state.
//!
//! Home is the new-agent composer: a prompt plus pickers for where and how to
//! run. Like the Projects panel, this module holds only state; the window
//! resolves the picker options from live daemon data, paints them, and routes
//! keys, so the model stays unit-testable without GPUI or a daemon.

use super::text_input::TextField;

/// The pickers in the composer.
///
/// `Agent` is state-only: the composer resolves it from the selected machine's
/// installed agents and shows no chip for it, so it is not part of `ALL`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HomeField {
    Agent,
    Model,
    Effort,
    Base,
    Branch,
    Project,
}

impl HomeField {
    /// Every picker the composer can focus, in chip order.
    ///
    /// `Agent` is state-only: the composer derives it from the selected
    /// machine's installed agents, so it no longer has a chip or a stop in the
    /// focus cycle.
    pub const ALL: [HomeField; 5] = [
        HomeField::Project,
        HomeField::Model,
        HomeField::Effort,
        HomeField::Base,
        HomeField::Branch,
    ];

    /// The chip label.
    pub fn label(self) -> &'static str {
        match self {
            HomeField::Agent => "Agent",
            HomeField::Model => "Model",
            HomeField::Effort => "Effort",
            HomeField::Base => "Base",
            HomeField::Branch => "Branch",
            HomeField::Project => "Project",
        }
    }
}

/// Where the composer's keyboard focus sits: the prompt or one picker.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum HomeFocus {
    /// The prompt edit area.
    #[default]
    Prompt,
    /// One picker chip.
    Picker(HomeField),
}

impl HomeFocus {
    /// Position in the prompt-plus-pickers cycle.
    fn index(self) -> usize {
        match self {
            Self::Prompt => 0,
            Self::Picker(field) => 1 + HomeField::ALL.iter().position(|f| *f == field).unwrap_or(0),
        }
    }

    fn from_index(index: usize) -> Self {
        if index == 0 {
            Self::Prompt
        } else {
            Self::Picker(HomeField::ALL[index - 1])
        }
    }

    /// The next focus, wrapping back to the prompt.
    pub fn next(self) -> Self {
        Self::from_index((self.index() + 1) % (HomeField::ALL.len() + 1))
    }

    /// The previous focus, wrapping forward to the last picker.
    pub fn prev(self) -> Self {
        let len = HomeField::ALL.len() + 1;
        Self::from_index((self.index() + len - 1) % len)
    }
}

/// What a launch starts from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BaseMode {
    /// Run in the project's existing checkout.
    Checkout,
    /// Create a worktree from the base branch first.
    NewWorktree,
}

impl BaseMode {
    /// Both modes, in toggle order.
    pub const ALL: [BaseMode; 2] = [BaseMode::Checkout, BaseMode::NewWorktree];

    /// The segment label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Checkout => "Current checkout",
            Self::NewWorktree => "New worktree",
        }
    }
}

/// One picker's option list and highlighted row.
#[derive(Clone, Debug, Default)]
pub struct Choice {
    options: Vec<String>,
    cursor: usize,
}

impl Choice {
    /// Every option.
    pub fn options(&self) -> &[String] {
        &self.options
    }

    /// The highlighted row.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// The highlighted option, if the list is not empty.
    pub fn selected(&self) -> Option<&str> {
        self.options.get(self.cursor).map(String::as_str)
    }

    /// Replace the options, keeping the highlight on the same value when it
    /// survives the change and clamping otherwise.
    pub fn set(&mut self, options: Vec<String>) {
        let keep = self.selected().map(str::to_string);
        self.options = options;
        self.cursor = keep
            .and_then(|value| self.options.iter().position(|option| *option == value))
            .unwrap_or(0);
    }

    /// Point the highlight at `index`, ignoring out-of-range rows.
    pub fn choose(&mut self, index: usize) {
        if index < self.options.len() {
            self.cursor = index;
        }
    }
}

/// The Home composer's state.
pub struct HomePanel {
    open: bool,
    prompt: TextField,
    focus: HomeFocus,
    agent: Choice,
    model: Choice,
    effort: Choice,
    base: Choice,
    branch: Choice,
    project: Choice,
    /// Which picker's dropdown is showing, if any.
    dropdown: Option<HomeField>,
    /// The picker an outside press just dismissed, so the chip's own click can
    /// tell a toggle-off from a fresh open.
    dismissed: Option<HomeField>,
    /// The open dropdown's type-to-filter query.
    filter: String,
    status: String,
}

impl Default for HomePanel {
    fn default() -> Self {
        let mut base = Choice::default();
        base.set(
            BaseMode::ALL
                .iter()
                .map(|mode| mode.label().to_string())
                .collect(),
        );
        Self {
            open: false,
            prompt: TextField::default(),
            focus: HomeFocus::Prompt,
            agent: Choice::default(),
            model: Choice::default(),
            effort: Choice::default(),
            base,
            branch: Choice::default(),
            project: Choice::default(),
            dropdown: None,
            dismissed: None,
            filter: String::new(),
            status: String::new(),
        }
    }
}

impl HomePanel {
    /// Whether the screen should paint in the card.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Show the screen, clearing the previous status and dropdown.
    pub fn open(&mut self) {
        self.open = true;
        self.dropdown = None;
        self.dismissed = None;
        self.filter.clear();
        self.focus = HomeFocus::Prompt;
        self.status.clear();
    }

    /// Hide the screen and forget transient state.
    pub fn close(&mut self) {
        self.open = false;
        self.dropdown = None;
        self.dismissed = None;
        self.filter.clear();
        self.status.clear();
    }

    /// The focused field.
    pub fn focus(&self) -> HomeFocus {
        self.focus
    }

    /// The focused picker, when a picker (not the prompt) has focus.
    pub fn focused_picker(&self) -> Option<HomeField> {
        match self.focus {
            HomeFocus::Picker(field) => Some(field),
            HomeFocus::Prompt => None,
        }
    }

    /// Focus `focus`.
    pub fn set_focus(&mut self, focus: HomeFocus) {
        self.focus = focus;
    }

    /// Move focus to the next field.
    pub fn focus_next(&mut self) {
        self.focus = self.focus.next();
    }

    /// Move focus to the previous field.
    pub fn focus_prev(&mut self) {
        self.focus = self.focus.prev();
    }

    /// The prompt buffer.
    pub fn prompt(&self) -> &TextField {
        &self.prompt
    }

    /// The picker options for `field`.
    pub fn choice(&self, field: HomeField) -> &Choice {
        match field {
            HomeField::Agent => &self.agent,
            HomeField::Model => &self.model,
            HomeField::Effort => &self.effort,
            HomeField::Base => &self.base,
            HomeField::Branch => &self.branch,
            HomeField::Project => &self.project,
        }
    }

    /// The picker options for `field`, mutably.
    pub fn choice_mut(&mut self, field: HomeField) -> &mut Choice {
        match field {
            HomeField::Agent => &mut self.agent,
            HomeField::Model => &mut self.model,
            HomeField::Effort => &mut self.effort,
            HomeField::Base => &mut self.base,
            HomeField::Branch => &mut self.branch,
            HomeField::Project => &mut self.project,
        }
    }

    /// What a launch starts from.
    pub fn base_mode(&self) -> BaseMode {
        if self.base.cursor() == 1 {
            BaseMode::NewWorktree
        } else {
            BaseMode::Checkout
        }
    }

    /// Whether the selected project is the personal-workspace row.
    pub fn project_is_none(&self) -> bool {
        self.project.cursor() == 0
    }

    /// The selected project's index in the caller's project list.
    pub fn project_index(&self) -> Option<usize> {
        (!self.project_is_none()).then(|| self.project.cursor() - 1)
    }

    /// Which picker's dropdown is showing, if any.
    pub fn dropdown(&self) -> Option<HomeField> {
        self.dropdown
    }

    /// Show `field`'s dropdown.
    pub fn open_dropdown(&mut self, field: HomeField) {
        self.dropdown = Some(field);
        self.filter.clear();
        self.clamp_dropdown_cursor();
    }

    /// Hide the dropdown without changing the selection.
    pub fn close_dropdown(&mut self) {
        self.dropdown = None;
        self.dismissed = None;
        self.filter.clear();
    }

    /// Close the dropdown because a press landed outside it.
    ///
    /// The chip's own click arrives on mouse-up, after this capture-phase
    /// handler, so the dismissed picker is remembered for that click to read.
    /// A press that lands while nothing is open just clears the memory.
    pub fn dismiss_dropdown(&mut self) {
        if self.dropdown.is_some() {
            self.dismissed = self.dropdown;
            self.dropdown = None;
            self.filter.clear();
        } else {
            self.dismissed = None;
        }
    }

    /// The picker an outside press dismissed, consuming the memory.
    pub fn take_dismissed(&mut self) -> Option<HomeField> {
        self.dismissed.take()
    }

    /// The open dropdown's filter query.
    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// Append typed text to the filter and re-point the highlight.
    pub fn filter_insert(&mut self, text: &str) {
        for ch in text.chars().filter(|ch| !ch.is_control()) {
            self.filter.push(ch);
        }
        self.clamp_dropdown_cursor();
    }

    /// Drop the filter's last character and re-point the highlight.
    pub fn filter_backspace(&mut self) {
        self.filter.pop();
        self.clamp_dropdown_cursor();
    }

    /// Indices of the open dropdown's options that match the filter.
    pub fn dropdown_matches(&self) -> Vec<usize> {
        let Some(field) = self.dropdown else {
            return Vec::new();
        };
        let needle = self.filter.to_lowercase();
        self.choice(field)
            .options()
            .iter()
            .enumerate()
            .filter(|(_, option)| needle.is_empty() || option.to_lowercase().contains(&needle))
            .map(|(index, _)| index)
            .collect()
    }

    /// Point the highlight at the first match when the filter excludes it.
    fn clamp_dropdown_cursor(&mut self) {
        let matches = self.dropdown_matches();
        let Some(field) = self.dropdown else {
            return;
        };
        if matches.is_empty() || matches.contains(&self.choice(field).cursor()) {
            return;
        }
        self.choice_mut(field).choose(matches[0]);
    }

    /// Move the open dropdown's highlight through the matching rows.
    pub fn move_dropdown(&mut self, delta: isize) {
        let matches = self.dropdown_matches();
        let Some(field) = self.dropdown else {
            return;
        };
        if matches.is_empty() {
            return;
        }
        let current = self.choice(field).cursor();
        let position = matches
            .iter()
            .position(|index| *index == current)
            .unwrap_or(0);
        let count = matches.len() as isize;
        let next = (position as isize + delta).rem_euclid(count) as usize;
        self.choice_mut(field).choose(matches[next]);
    }

    /// Point the open dropdown at `index`.
    pub fn choose_dropdown(&mut self, index: usize) {
        if let Some(field) = self.dropdown {
            self.choice_mut(field).choose(index);
        }
    }

    /// The last status line.
    pub fn status(&self) -> &str {
        &self.status
    }

    /// Show a one-line status or error in the composer footer.
    pub fn set_status(&mut self, status: String) {
        self.status = status;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choice_set_keeps_value_when_it_survives() {
        let mut choice = Choice::default();
        choice.set(vec!["a".into(), "b".into(), "c".into()]);
        choice.choose(1);
        choice.set(vec!["c".into(), "b".into()]);
        assert_eq!(choice.selected(), Some("b"));
        choice.set(vec!["x".into(), "y".into()]);
        assert_eq!(choice.selected(), Some("x"));
    }

    #[test]
    fn outside_press_dismisses_and_the_chip_click_sees_it() {
        let mut home = HomePanel::default();
        home.open_dropdown(HomeField::Model);
        home.dismiss_dropdown();
        assert_eq!(home.dropdown(), None);
        // The chip's click arrives after the press and reads the dismissal.
        assert_eq!(home.take_dismissed(), Some(HomeField::Model));
        // A press with nothing open forgets the previous dismissal.
        home.dismiss_dropdown();
        assert_eq!(home.take_dismissed(), None);
    }

    #[test]
    fn focus_cycles_through_prompt_and_pickers() {
        let mut home = HomePanel::default();
        assert_eq!(home.focus(), HomeFocus::Prompt);
        home.focus_prev();
        assert_eq!(home.focus(), HomeFocus::Picker(HomeField::Branch));
        home.focus_next();
        assert_eq!(home.focus(), HomeFocus::Prompt);
        home.focus_next();
        assert_eq!(home.focus(), HomeFocus::Picker(HomeField::Project));
    }

    #[test]
    fn dropdown_filter_narrows_and_rehomes_the_cursor() {
        let mut home = HomePanel::default();
        home.choice_mut(HomeField::Model)
            .set(vec!["Alpha".into(), "Beta".into(), "Alpine".into()]);
        home.choice_mut(HomeField::Model).choose(1);
        home.open_dropdown(HomeField::Model);
        home.filter_insert("al");
        assert_eq!(home.dropdown_matches(), vec![0, 2]);
        assert_eq!(home.choice(HomeField::Model).cursor(), 0);
        home.move_dropdown(1);
        assert_eq!(home.choice(HomeField::Model).cursor(), 2);
        home.filter_backspace();
        assert_eq!(home.dropdown_matches(), vec![0, 1, 2]);
    }

    #[test]
    fn project_none_maps_to_no_index() {
        let mut home = HomePanel::default();
        home.choice_mut(HomeField::Project)
            .set(vec!["No project".into(), "flo".into()]);
        assert!(home.project_is_none());
        assert_eq!(home.project_index(), None);
        home.choice_mut(HomeField::Project).choose(1);
        assert_eq!(home.project_index(), Some(0));
    }

    #[test]
    fn base_mode_reads_the_picker() {
        let mut home = HomePanel::default();
        assert_eq!(home.base_mode(), BaseMode::Checkout);
        home.choice_mut(HomeField::Base).choose(1);
        assert_eq!(home.base_mode(), BaseMode::NewWorktree);
    }
}
