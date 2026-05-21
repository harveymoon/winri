#[derive(Debug, Clone)]
pub enum Action {
    Tiler(TilerAction),
    Overview(OverviewAction),
    OpenSettings,
    Exit,
}

#[derive(Debug, Clone)]
pub enum TilerAction {
    CloseCurrent,
    MoveFocusNext,
    MoveFocusPrevious,
    SwapWithNext,
    SwapWithPrevious,
    ResizeToFullscreen,
    ResizeToHalfScreen,
    IncrementWidth,
    DecrementWidth,
    OpenOverview,
    ForceRefresh,
    CenterFocused,
    /// Add the currently-focused window's class to the persistent
    /// `ignored_classes` list and save the config. Designed for transient
    /// popups (TouchDesigner's Op Create Dialog, etc.) that dismiss on
    /// mouse-click, so the overview right-click flow can't reach them.
    IgnoreFocusedWindow,
}

#[derive(Debug, Clone)]
pub enum OverviewAction {
    CloseOverview,
    JumpTo(crate::window::Window),
}
