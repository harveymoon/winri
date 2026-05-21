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
}

#[derive(Debug, Clone)]
pub enum OverviewAction {
    CloseOverview,
    JumpTo(crate::window::Window),
}
