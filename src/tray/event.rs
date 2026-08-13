#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    Show,
    MenuShow,
    MenuQuit,
    PowerResumed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayCommand {
    CreateIcon,
    RemoveIcon,
    Shutdown,
}

pub const ID_SHOW: u32 = 1;
pub const ID_QUIT: u32 = 2;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tray_event_equality() {
        assert_eq!(TrayEvent::Show, TrayEvent::Show);
        assert_eq!(TrayEvent::MenuShow, TrayEvent::MenuShow);
        assert_eq!(TrayEvent::MenuQuit, TrayEvent::MenuQuit);
        assert_ne!(TrayEvent::Show, TrayEvent::MenuShow);
        assert_ne!(TrayEvent::Show, TrayEvent::MenuQuit);
        assert_ne!(TrayEvent::MenuShow, TrayEvent::MenuQuit);
    }

    #[test]
    fn tray_event_clone() {
        let e = TrayEvent::Show;
        let e2 = e;
        assert_eq!(e, e2);
    }

    #[test]
    fn tray_command_equality() {
        assert_eq!(TrayCommand::CreateIcon, TrayCommand::CreateIcon);
        assert_eq!(TrayCommand::RemoveIcon, TrayCommand::RemoveIcon);
        assert_eq!(TrayCommand::Shutdown, TrayCommand::Shutdown);
        assert_ne!(TrayCommand::CreateIcon, TrayCommand::RemoveIcon);
        assert_ne!(TrayCommand::CreateIcon, TrayCommand::Shutdown);
        assert_ne!(TrayCommand::RemoveIcon, TrayCommand::Shutdown);
    }

    #[test]
    fn tray_command_clone() {
        let c = TrayCommand::Shutdown;
        let c2 = c;
        assert_eq!(c, c2);
    }

    #[test]
    fn tray_constants() {
        assert_eq!(ID_SHOW, 1);
        assert_eq!(ID_QUIT, 2);
    }
}
