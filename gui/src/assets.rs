//! The app's own assets, ahead of the bundled icon catalog.
//!
//! The kage glyph, the kanji the web client draws its wordmark with,
//! is not a Lucide icon, so the catalog does not carry it. This source
//! serves the glyph from an embedded copy and hands every other path
//! to the toolkit's catalog, which keeps loading icons the way each
//! platform already does: embedded on the desktop, fetched from the
//! page origin in the browser.

use std::borrow::Cow;

use gpui_kit::assets::Assets;
use gpui_kit::{AssetSource, Result, SharedString};

/// The asset path the kage glyph outline lives under.
pub const GLYPH_PATH: &str = "icons/kage-glyph.svg";

/// The gradient-filled glyph for the dark kage palette: the same
/// outline the wordmark's face layer draws, pre-rendered at 8x with
/// the design's orb-1 to orb-2 diagonal, because the toolkit's SVG
/// element paints one flat color only.
pub const GLYPH_FACE_SHADOW: &str = "brand/kage-glyph-shadow.png";

/// The gradient-filled glyph for the light kage-dawn palette.
pub const GLYPH_FACE_DAWN: &str = "brand/kage-glyph-dawn.png";

/// The welcome pane's radial glow for the dark kage palette: the web
/// client's `radial-gradient(ellipse 50% 40% at 50% 38%)`, pre-rendered
/// in element-relative space and stretched to the pane at paint time,
/// because the toolkit has no radial-gradient background.
pub const WELCOME_GLOW: &str = "brand/welcome-glow.png";

/// The welcome glow for the light kage-dawn palette.
pub const WELCOME_GLOW_DAWN: &str = "brand/welcome-glow-dawn.png";

/// The glyph outline bytes: the kanji path the web client's `dom.js`
/// carries, from Noto Serif CJK JP Bold (SIL OFL 1.1).
static GLYPH: &[u8] = include_bytes!("../assets/brand/kage-glyph.svg");

/// The dark-palette gradient face bytes, rendered by `resvg` from the
/// same outline and the palette's orb stops.
pub(crate) static GLYPH_FACE_SHADOW_PNG: &[u8] =
    include_bytes!("../assets/brand/kage-glyph-shadow.png");

/// The dawn-palette gradient face bytes.
static GLYPH_FACE_DAWN_PNG: &[u8] = include_bytes!("../assets/brand/kage-glyph-dawn.png");

/// The dark-palette welcome glow bytes.
static WELCOME_GLOW_PNG: &[u8] = include_bytes!("../assets/brand/welcome-glow.png");

/// The dawn-palette welcome glow bytes.
static WELCOME_GLOW_DAWN_PNG: &[u8] = include_bytes!("../assets/brand/welcome-glow-dawn.png");

// Every icon the app draws, and the toolkit components' own defaults,
// embedded in both builds. The browser catalog fetches an icon on first
// use and fails that first load, which leaves an icon drawn once blank;
// these never touch the network. A new `IconName` the app draws belongs
// in this list.
gpui_kit::assets::icon_assets!(
    AppIcons,
    [
        ALargeSmall,
        Archive,
        ArrowDown,
        ArrowLeft,
        ArrowRight,
        ArrowUp,
        Asterisk,
        AtSign,
        Ban,
        Battery,
        BatteryCharging,
        BatteryFull,
        BatteryLow,
        BatteryMedium,
        BatteryWarning,
        Bell,
        BookOpen,
        Bot,
        Building2,
        Calendar,
        CaseSensitive,
        ChartPie,
        Check,
        ChevronDown,
        ChevronLeft,
        ChevronRight,
        ChevronUp,
        ChevronsUpDown,
        Circle,
        CircleAlert,
        CircleCheck,
        CirclePause,
        CircleSlash,
        CircleUser,
        CircleX,
        Close,
        Command,
        Copy,
        CornerDownLeft,
        CornerDownRight,
        Cpu,
        Dash,
        Delete,
        Download,
        Ellipsis,
        EllipsisVertical,
        ExternalLink,
        Eye,
        EyeOff,
        File,
        FileDiff,
        FilePlus,
        FileText,
        Folder,
        FolderClosed,
        FolderLock,
        FolderOpen,
        FolderUp,
        Frame,
        GalleryVerticalEnd,
        GitBranch,
        GitFork,
        Github,
        Globe,
        Hand,
        HardDrive,
        Heart,
        HeartOff,
        House,
        Inbox,
        Info,
        Inspector,
        Layers,
        LayoutDashboard,
        Lightbulb,
        List,
        ListTodo,
        ListTree,
        Loader,
        LoaderCircle,
        Map,
        Maximize,
        MemoryStick,
        Menu,
        MessageSquare,
        Minimize,
        Minus,
        Moon,
        Network,
        Palette,
        PanelBottom,
        PanelBottomOpen,
        PanelLeft,
        PanelLeftClose,
        PanelLeftOpen,
        PanelRight,
        PanelRightClose,
        PanelRightOpen,
        Paperclip,
        Pause,
        PenLine,
        Pencil,
        Pin,
        PinOff,
        Play,
        Plus,
        Redo,
        Redo2,
        RefreshCw,
        Replace,
        ResizeCorner,
        RotateCw,
        Search,
        Server,
        Settings,
        Settings2,
        ShieldAlert,
        ShieldCheck,
        ShieldQuestionMark,
        ShieldX,
        Slash,
        SortAscending,
        SortDescending,
        Square,
        SquarePen,
        SquareTerminal,
        Star,
        StarFill,
        StarOff,
        Sun,
        Target,
        Terminal,
        ThumbsDown,
        ThumbsUp,
        Trash,
        TriangleAlert,
        Undo,
        Undo2,
        User,
        Users,
        Waypoints,
        WindowClose,
        WindowMaximize,
        WindowMinimize,
        WindowRestore,
        X,
        Zap,
    ]
);

/// The app asset source: the glyph faces first, the embedded icons
/// next, the bundled catalog last.
pub struct KageAssets {
    catalog: Assets,
}

impl KageAssets {
    /// A source over the toolkit's catalog.
    #[must_use]
    pub fn new(catalog: Assets) -> Self {
        Self { catalog }
    }
}

impl AssetSource for KageAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        match path {
            GLYPH_PATH => Ok(Some(Cow::Borrowed(GLYPH))),
            GLYPH_FACE_SHADOW => Ok(Some(Cow::Borrowed(GLYPH_FACE_SHADOW_PNG))),
            GLYPH_FACE_DAWN => Ok(Some(Cow::Borrowed(GLYPH_FACE_DAWN_PNG))),
            WELCOME_GLOW => Ok(Some(Cow::Borrowed(WELCOME_GLOW_PNG))),
            WELCOME_GLOW_DAWN => Ok(Some(Cow::Borrowed(WELCOME_GLOW_DAWN_PNG))),
            _ => match AppIcons.load(path)? {
                Some(icon) => Ok(Some(icon)),
                None => self.catalog.load(path),
            },
        }
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        self.catalog.list(path)
    }
}
