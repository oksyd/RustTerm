//! Layout stores identities, never transports. Reordering cannot reconnect a PTY.
pub struct WorkspaceLayout {
    pub sidebar_fits: bool,
    pub sidebar_visible: bool,
    pub sidebar_inline: bool,
    pub sidebar_width: f32,
    pub files_docked: bool,
    pub files_width: f32,
}
impl WorkspaceLayout {
    pub fn new(
        width: f32,
        settings: &rustterm::config::Settings,
        sidebar_overlay: bool,
        content_minimum: f32,
        utility_open: bool,
    ) -> Self {
        let content_minimum = content_minimum.max(360.);
        let sidebar_width = settings.sidebar_width.min((width - 80.).max(0.));
        let sidebar_fits =
            width >= sidebar_width + 5. + content_minimum + if utility_open { 365. } else { 0. };
        let sidebar_inline = sidebar_fits && settings.sidebar_visible;
        let sidebar_visible = sidebar_inline || (!sidebar_fits && sidebar_overlay);
        let available = width
            - if sidebar_inline {
                sidebar_width + 5.
            } else {
                0.
            };
        let files_docked = available >= content_minimum + 365.;
        Self {
            sidebar_fits,
            sidebar_visible,
            sidebar_inline,
            sidebar_width,
            files_docked,
            files_width: settings
                .files_width
                .min((available - content_minimum - 5.).max(360.)),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
    Horizontal,
    Vertical,
}

#[derive(Clone, Debug)]
pub enum PaneNode {
    Leaf(usize),
    Split {
        id: usize,
        axis: Axis,
        ratio: f32,
        first: Box<PaneNode>,
        second: Box<PaneNode>,
    },
}
impl PaneNode {
    /// Adapt a render copy to the current extent. User-chosen ratios are kept in
    /// the original tree and restored as soon as enough space becomes available.
    pub fn fit(&mut self, size: (f32, f32), leaf: &impl Fn(usize) -> (f32, f32)) {
        if let Self::Split {
            axis,
            ratio,
            first,
            second,
            ..
        } = self
        {
            let a = first.minimum_size(leaf);
            let b = second.minimum_size(leaf);
            let (extent, minimum_a, minimum_b) = match axis {
                Axis::Horizontal => (size.0, a.0, b.0),
                Axis::Vertical => (size.1, a.1, b.1),
            };
            if extent >= minimum_a + minimum_b + 5. {
                *ratio = (*ratio * extent)
                    .clamp(minimum_a, (extent - minimum_b - 5.).max(minimum_a))
                    / extent;
            }
            let first_extent = extent * *ratio;
            let second_extent = (extent - first_extent - 5.).max(0.);
            let (first_size, second_size) = match axis {
                Axis::Horizontal => ((first_extent, size.1), (second_extent, size.1)),
                Axis::Vertical => ((size.0, first_extent), (size.0, second_extent)),
            };
            first.fit(first_size, leaf);
            second.fit(second_size, leaf);
        }
    }
    pub fn pane_size(&self, target: usize, size: (f32, f32)) -> Option<(f32, f32)> {
        match self {
            Self::Leaf(id) => (*id == target).then_some(size),
            Self::Split {
                axis,
                ratio,
                first,
                second,
                ..
            } => {
                let (a, b) = match axis {
                    Axis::Horizontal => (
                        (size.0 * ratio, size.1),
                        ((size.0 * (1. - ratio) - 5.).max(0.), size.1),
                    ),
                    Axis::Vertical => (
                        (size.0, size.1 * ratio),
                        (size.0, (size.1 * (1. - ratio) - 5.).max(0.)),
                    ),
                };
                first
                    .pane_size(target, a)
                    .or_else(|| second.pane_size(target, b))
            }
        }
    }
    pub fn leaves(&self) -> Vec<usize> {
        match self {
            Self::Leaf(id) => vec![*id],
            Self::Split { first, second, .. } => {
                let mut ids = first.leaves();
                ids.extend(second.leaves());
                ids
            }
        }
    }
    pub fn split(
        &mut self,
        target: usize,
        new_pane: usize,
        split_id: usize,
        axis: Axis,
        ratio: f32,
    ) -> bool {
        if !ratio.is_finite() || ratio <= 0. || ratio >= 1. {
            return false;
        }
        match self {
            Self::Leaf(id) if *id == target => {
                *self = Self::Split {
                    id: split_id,
                    axis,
                    ratio,
                    first: Box::new(Self::Leaf(target)),
                    second: Box::new(Self::Leaf(new_pane)),
                };
                true
            }
            Self::Split { first, second, .. } => {
                first.split(target, new_pane, split_id, axis, ratio)
                    || second.split(target, new_pane, split_id, axis, ratio)
            }
            _ => false,
        }
    }
    pub fn remove(self, target: usize) -> Option<Self> {
        match self {
            Self::Leaf(id) => (id != target).then_some(Self::Leaf(id)),
            Self::Split {
                id,
                axis,
                ratio,
                first,
                second,
            } => match (first.remove(target), second.remove(target)) {
                (Some(first), Some(second)) => Some(Self::Split {
                    id,
                    axis,
                    ratio,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (Some(node), None) | (None, Some(node)) => Some(node),
                (None, None) => None,
            },
        }
    }
    pub fn minimum_size(&self, leaf: &impl Fn(usize) -> (f32, f32)) -> (f32, f32) {
        match self {
            Self::Leaf(id) => leaf(*id),
            Self::Split {
                axis,
                first,
                second,
                ..
            } => {
                let a = first.minimum_size(leaf);
                let b = second.minimum_size(leaf);
                match axis {
                    Axis::Horizontal => (a.0 + b.0 + 5., a.1.max(b.1)),
                    Axis::Vertical => (a.0.max(b.0), a.1 + b.1 + 5.),
                }
            }
        }
    }
    pub fn set_ratio(
        &mut self,
        target: usize,
        value: f32,
        extent: f32,
        leaf: &impl Fn(usize) -> (f32, f32),
    ) {
        if let Self::Split {
            id,
            axis,
            ratio,
            first,
            second,
            ..
        } = self
        {
            if *id == target {
                let a = first.minimum_size(leaf);
                let b = second.minimum_size(leaf);
                let (a, b) = match axis {
                    Axis::Horizontal => (a.0, b.0),
                    Axis::Vertical => (a.1, b.1),
                };
                if extent.is_finite() && extent >= a + b + 5. && value.is_finite() {
                    *ratio = (value * extent).clamp(a, (extent - b - 5.).max(a)) / extent;
                }
            } else {
                first.set_ratio(target, value, extent, leaf);
                second.set_ratio(target, value, extent, leaf);
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn library_collapses_before_the_session_utility() {
        let settings = rustterm::config::Settings::default();
        let terminal_only = WorkspaceLayout::new(900., &settings, false, 460., false);
        assert!(terminal_only.sidebar_inline);
        let with_tools = WorkspaceLayout::new(900., &settings, false, 460., true);
        assert!(!with_tools.sidebar_inline);
        assert!(with_tools.files_docked);
        let narrow = WorkspaceLayout::new(800., &settings, false, 460., true);
        assert!(!narrow.sidebar_inline && !narrow.files_docked);
        let wide = WorkspaceLayout::new(1200., &settings, false, 460., true);
        assert!(wide.sidebar_inline && wide.files_docked);
    }
    #[test]
    fn narrow_windows_use_overlays_without_changing_panel_preferences() {
        let settings = rustterm::config::Settings::default();
        let narrow = WorkspaceLayout::new(480., &settings, false, 360., true);
        assert!(!narrow.sidebar_visible);
        assert!(!narrow.files_docked);
        let expanded = WorkspaceLayout::new(480., &settings, true, 360., true);
        assert!(expanded.sidebar_visible);
        assert!(!expanded.sidebar_inline);
        let wide = WorkspaceLayout::new(1100., &settings, false, 360., true);
        assert!(wide.sidebar_inline && wide.files_docked);
        assert_eq!(wide.sidebar_width, settings.sidebar_width);
        assert!(1100. - wide.sidebar_width - wide.files_width - 10. >= 360.);
    }
    #[test]
    fn dock_clamps_to_available_space_and_restores_saved_width() {
        let settings = rustterm::config::Settings {
            files_width: 800.,
            sidebar_visible: false,
            ..Default::default()
        };
        assert!(!WorkspaceLayout::new(724., &settings, false, 360., true).files_docked);
        assert_eq!(
            WorkspaceLayout::new(725., &settings, false, 360., true).files_width,
            360.
        );
        assert_eq!(
            WorkspaceLayout::new(1600., &settings, false, 360., true).files_width,
            800.
        );
    }
    #[test]
    fn sidebar_yields_to_the_actual_workspace_minimum() {
        let settings = rustterm::config::Settings {
            sidebar_width: 400.,
            ..Default::default()
        };
        let layout = WorkspaceLayout::new(760., &settings, false, 460., true);
        assert!(!layout.sidebar_inline);
        let mixed = WorkspaceLayout::new(1100., &settings, false, 805., true);
        assert!(!mixed.sidebar_inline);
        assert!(!mixed.files_docked);
        let wide = WorkspaceLayout::new(1600., &settings, false, 805., true);
        assert!(wide.sidebar_inline && wide.files_docked);
    }
    #[test]
    fn render_fit_preserves_mixed_minimums_and_saved_ratios() {
        let minimum = |id| if id == 1 { (240., 160.) } else { (460., 260.) };
        let mut saved = PaneNode::Leaf(1);
        saved.split(1, 2, 3, Axis::Horizontal, 0.7);
        let mut rendered = saved.clone();
        rendered.fit((800., 500.), &minimum);
        assert!((rendered.pane_size(2, (800., 500.)).unwrap().0 - 460.).abs() < 0.01);
        assert_eq!(saved.pane_size(1, (800., 500.)).unwrap().0, 560.);
    }
    #[test]
    fn removing_nested_pane_collapses_only_its_parent() {
        let mut n = PaneNode::Leaf(1);
        assert!(n.split(1, 2, 10, Axis::Horizontal, 0.5));
        assert!(n.split(2, 3, 11, Axis::Vertical, 0.5));
        let n = n.remove(2).unwrap();
        assert_eq!(n.leaves(), vec![1, 3]);
        assert_eq!(n.remove(1).unwrap().leaves(), vec![3]);
    }
    #[test]
    fn split_missing_pane_does_not_orphan_a_leaf() {
        let mut n = PaneNode::Leaf(1);
        assert!(!n.split(9, 2, 10, Axis::Horizontal, 0.5));
        assert_eq!(n.leaves(), vec![1]);
    }
    #[test]
    fn zoom_split_validation_uses_the_original_nested_pane_dimensions() {
        let mut layout = PaneNode::Leaf(1);
        layout.split(1, 2, 10, Axis::Horizontal, 0.5);
        layout.split(2, 3, 11, Axis::Vertical, 0.5);
        assert_eq!(layout.pane_size(1, (1000., 600.)), Some((500., 600.)));
        assert_eq!(layout.pane_size(3, (1000., 600.)), Some((495., 295.)));
        assert_eq!(layout.pane_size(9, (1000., 600.)), None);
    }
    #[test]
    fn resize_preserves_nested_minimum_sizes() {
        let mut n = PaneNode::Leaf(1);
        n.split(1, 2, 10, Axis::Horizontal, 0.5);
        n.split(2, 3, 11, Axis::Horizontal, 0.5);
        assert_eq!(n.minimum_size(&|_| (200., 100.)), (610., 100.));
        n.set_ratio(10, 0.99, 1000., &|_| (200., 100.));
        let PaneNode::Split { ratio, .. } = n else {
            unreachable!()
        };
        assert!((ratio - 0.59).abs() < 0.0001);
    }
    #[test]
    fn mixed_pane_sizes_clamp_each_side_independently() {
        let minimum = |id| if id == 1 { (200., 100.) } else { (460., 300.) };
        let mut layout = PaneNode::Leaf(1);
        assert!(layout.split(1, 2, 10, Axis::Horizontal, 0.3));
        assert_eq!(layout.minimum_size(&minimum), (665., 300.));
        assert_eq!(layout.pane_size(1, (1000., 600.)), Some((300., 600.)));
        layout.set_ratio(10, 0.99, 1000., &minimum);
        let redis = layout.pane_size(2, (1000., 600.)).unwrap();
        assert!((redis.0 - 460.).abs() < 0.001);
        layout.set_ratio(10, 0.01, 1000., &minimum);
        assert_eq!(layout.pane_size(1, (1000., 600.)), Some((200., 600.)));
        let unchanged = layout.pane_size(1, (1000., 600.));
        layout.set_ratio(10, 0.8, 664., &minimum);
        assert_eq!(layout.pane_size(1, (1000., 600.)), unchanged);
        assert!(!layout.split(2, 3, 11, Axis::Vertical, f32::NAN));
        assert_eq!(layout.leaves(), vec![1, 2]);
    }
}
