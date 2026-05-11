//! Compose panes into back surface.

use crate::term::pane::Pane;
use crate::term::surface::Surface;

pub fn compose(panes: &[Pane], back: &mut Surface) {
    if let Some(pane) = panes.first() {
        pane.cells_into(back, 0, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::compose;
    use crate::backend::PaneId;
    use crate::term::pane::Pane;
    use crate::term::surface::Surface;

    #[test]
    fn compose_blits_first_pane_fullscreen() {
        let mut surface = Surface::new(80, 24);
        let mut pane = Pane::new(PaneId(1), 80, 24);

        pane.process(b"hi");
        compose(&[pane], &mut surface);

        assert_eq!(surface.get(0, 0).expect("cell exists").ch, 'h');
        assert_eq!(surface.get(1, 0).expect("cell exists").ch, 'i');
    }
}
