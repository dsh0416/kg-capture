mod connection;
mod system_fonts;

use std::sync::{Arc, Mutex};

use connection::Session;
use iced::futures::SinkExt;
use iced::widget::{button, checkbox, column, container, pick_list, row, slider, text, text_input};
use iced::{Color, Element, Fill, Font, Point, Size, Subscription, Task, Theme, window};
use ipc_channel::ipc::IpcReceiver;
use kg_capture_protocol::{HookEvent, HostCommand, LyricLine, LyricTimeline, PlaybackPosition};

fn main() -> iced::Result {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "kg_capture=info".into()),
        )
        .init();

    iced::daemon(App::new, App::update, App::view)
        .title(App::title)
        .theme(App::theme)
        .subscription(App::subscription)
        .run()
}

#[derive(Debug, Clone)]
enum Message {
    BrowseExecutable,
    ExecutableSelected(Option<std::path::PathBuf>),
    ExecutablePathChanged(String),
    Launch,
    Connected(Result<Session, String>),
    HookEvent(Result<HookEvent, String>),
    Disconnect,
    ShowLyricsWindow,
    BackgroundColorChanged(String),
    TextColorChanged(String),
    HighlightColorChanged(String),
    LyricsFontChanged(LyricsFont),
    LyricsAlignmentChanged(LyricsAlignment),
    ActiveFontSizeChanged(f32),
    CandidateFontSizeChanged(f32),
    ShowPreviousLineChanged(bool),
    CandidateLineCountChanged(f32),
    WindowCloseRequested(window::Id),
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum ConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Streaming,
    Failed,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum LyricsFont {
    System,
    Named {
        family_name: &'static str,
        display_name: &'static str,
    },
}

impl LyricsFont {
    fn font(self) -> Font {
        match self {
            Self::System => Font::DEFAULT,
            Self::Named { family_name, .. } => Font::with_name(family_name),
        }
    }
}

impl std::fmt::Display for LyricsFont {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::System => "系统默认",
            Self::Named { display_name, .. } => display_name,
        })
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum LyricsAlignment {
    Left,
    Center,
    Right,
}

impl LyricsAlignment {
    const ALL: [Self; 3] = [Self::Left, Self::Center, Self::Right];

    fn horizontal(self) -> iced::alignment::Horizontal {
        match self {
            Self::Left => iced::alignment::Horizontal::Left,
            Self::Center => iced::alignment::Horizontal::Center,
            Self::Right => iced::alignment::Horizontal::Right,
        }
    }
}

impl std::fmt::Display for LyricsAlignment {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Left => "左对齐",
            Self::Center => "居中",
            Self::Right => "右对齐",
        })
    }
}

fn available_lyrics_fonts() -> Vec<LyricsFont> {
    let mut fonts = vec![LyricsFont::System];
    match system_fonts::families() {
        Ok(names) => fonts.extend(names.into_iter().map(|font| LyricsFont::Named {
            family_name: Box::leak(font.family_name.into_boxed_str()),
            display_name: Box::leak(font.display_name.into_boxed_str()),
        })),
        Err(error) => tracing::warn!(%error, "failed to enumerate DirectWrite font families"),
    }
    fonts
}

fn preferred_lyrics_font(fonts: &[LyricsFont]) -> LyricsFont {
    fonts
        .iter()
        .copied()
        .find(|font| {
            matches!(
                font,
                LyricsFont::Named {
                    family_name: "Microsoft YaHei",
                    ..
                }
            )
        })
        .unwrap_or(LyricsFont::System)
}

struct LyricsAppearance {
    background_input: String,
    background: Color,
    text_input: String,
    text: Color,
    highlight_input: String,
    highlight: Color,
    font: LyricsFont,
    alignment: LyricsAlignment,
    active_font_size: f32,
    candidate_font_size: f32,
    show_previous_line: bool,
    candidate_line_count: usize,
}

impl Default for LyricsAppearance {
    fn default() -> Self {
        Self {
            background_input: "#292B2F".into(),
            background: Color::from_rgb8(0x29, 0x2b, 0x2f),
            text_input: "#F5F5F5".into(),
            text: Color::from_rgb8(0xf5, 0xf5, 0xf5),
            highlight_input: "#FFD54F".into(),
            highlight: Color::from_rgb8(0xff, 0xd5, 0x4f),
            font: LyricsFont::System,
            alignment: LyricsAlignment::Center,
            active_font_size: 38.0,
            candidate_font_size: 24.0,
            show_previous_line: true,
            candidate_line_count: 3,
        }
    }
}

struct App {
    control_window: window::Id,
    lyrics_window: Option<window::Id>,
    connection: ConnectionState,
    detail: String,
    session: Option<Session>,
    timeline: Option<LyricTimeline>,
    playback: Option<PlaybackPosition>,
    executable_path: String,
    available_fonts: Vec<LyricsFont>,
    lyrics_appearance: LyricsAppearance,
}

impl App {
    fn new() -> (Self, Task<Message>) {
        let available_fonts = available_lyrics_fonts();
        let lyrics_appearance = LyricsAppearance {
            font: preferred_lyrics_font(&available_fonts),
            ..LyricsAppearance::default()
        };
        let (control_window, open_control_window) = window::open(window::Settings {
            size: Size::new(720.0, 680.0),
            min_size: Some(Size::new(640.0, 620.0)),
            position: window::Position::Specific(Point::new(40.0, 40.0)),
            exit_on_close_request: false,
            ..window::Settings::default()
        });
        let (lyrics_window, open_lyrics_task) = open_lyrics_window();

        (
            Self {
                control_window,
                lyrics_window: Some(lyrics_window),
                connection: ConnectionState::Disconnected,
                detail: "选择 WeSing.exe；程序将以子进程方式启动并读取歌词。".into(),
                session: None,
                timeline: None,
                playback: None,
                executable_path: String::new(),
                available_fonts,
                lyrics_appearance,
            },
            Task::batch([open_control_window.discard(), open_lyrics_task.discard()]),
        )
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::BrowseExecutable => Task::perform(
                async {
                    rfd::FileDialog::new()
                        .set_title("Locate the WeSing executable")
                        .add_filter("Windows executable", &["exe"])
                        .pick_file()
                },
                Message::ExecutableSelected,
            ),
            Message::ExecutableSelected(path) => {
                if let Some(path) = path {
                    self.executable_path = path.to_string_lossy().into_owned();
                    self.detail = "已选择目标程序，可以启动。".into();
                }
                Task::none()
            }
            Message::ExecutablePathChanged(path) => {
                self.executable_path = path;
                Task::none()
            }
            Message::Launch => {
                self.connection = ConnectionState::Connecting;
                self.detail = "正在启动 WeSing 并初始化歌词同步…".into();
                let executable = std::path::PathBuf::from(self.executable_path.clone());
                Task::perform(
                    async move { Session::connect(executable) },
                    Message::Connected,
                )
            }
            Message::Connected(Ok(session)) => match session.send(HostCommand::StartCapture) {
                Ok(()) => {
                    self.connection = ConnectionState::Connected;
                    self.detail =
                        format!("已连接到进程 {}，正在初始化歌词同步…", session.process_id);
                    self.session = Some(session.clone());
                    Task::run(event_stream(session.event_receiver), Message::HookEvent)
                }
                Err(error) => {
                    self.connection = ConnectionState::Failed;
                    self.detail = error;
                    Task::none()
                }
            },
            Message::Connected(Err(error)) => {
                self.connection = ConnectionState::Failed;
                self.detail = error;
                Task::none()
            }
            Message::Disconnect => {
                self.send(HostCommand::Shutdown);
                self.session = None;
                self.connection = ConnectionState::Disconnected;
                self.detail = "已断开连接。".into();
                self.timeline = None;
                self.playback = None;
                Task::none()
            }
            Message::ShowLyricsWindow => {
                if let Some(window) = self.lyrics_window {
                    window::gain_focus(window)
                } else {
                    let (window, open_window) = open_lyrics_window();
                    self.lyrics_window = Some(window);
                    open_window.discard()
                }
            }
            Message::BackgroundColorChanged(input) => {
                if let Some(color) = parse_hex_color(&input) {
                    self.lyrics_appearance.background = color;
                }
                self.lyrics_appearance.background_input = input;
                Task::none()
            }
            Message::TextColorChanged(input) => {
                if let Some(color) = parse_hex_color(&input) {
                    self.lyrics_appearance.text = color;
                }
                self.lyrics_appearance.text_input = input;
                Task::none()
            }
            Message::HighlightColorChanged(input) => {
                if let Some(color) = parse_hex_color(&input) {
                    self.lyrics_appearance.highlight = color;
                }
                self.lyrics_appearance.highlight_input = input;
                Task::none()
            }
            Message::LyricsFontChanged(font) => {
                self.lyrics_appearance.font = font;
                Task::none()
            }
            Message::LyricsAlignmentChanged(alignment) => {
                self.lyrics_appearance.alignment = alignment;
                Task::none()
            }
            Message::ActiveFontSizeChanged(size) => {
                self.lyrics_appearance.active_font_size = size.clamp(20.0, 96.0);
                Task::none()
            }
            Message::CandidateFontSizeChanged(size) => {
                self.lyrics_appearance.candidate_font_size = size.clamp(12.0, 72.0);
                Task::none()
            }
            Message::ShowPreviousLineChanged(show) => {
                self.lyrics_appearance.show_previous_line = show;
                Task::none()
            }
            Message::CandidateLineCountChanged(count) => {
                self.lyrics_appearance.candidate_line_count = count.clamp(0.0, 10.0) as usize;
                Task::none()
            }
            Message::WindowCloseRequested(window) if window == self.control_window => {
                if let Some(session) = &self.session {
                    let _ = session.send(HostCommand::Shutdown);
                }
                iced::exit()
            }
            Message::WindowCloseRequested(window) => {
                if self.lyrics_window == Some(window) {
                    self.lyrics_window = None;
                }
                window::close(window)
            }
            Message::HookEvent(Ok(event)) => {
                if let Some(session) = &self.session {
                    session.log_event(&event);
                }
                self.handle_hook_event(event);
                Task::none()
            }
            Message::HookEvent(Err(error)) => {
                self.session = None;
                self.connection = ConnectionState::Failed;
                self.detail = error;
                Task::none()
            }
        }
    }

    fn title(&self, window: window::Id) -> String {
        if window == self.control_window {
            "KG Capture".into()
        } else {
            "KG Lyrics".into()
        }
    }

    fn theme(&self, _window: window::Id) -> Theme {
        Theme::Dark
    }

    fn subscription(&self) -> Subscription<Message> {
        window::close_requests().map(Message::WindowCloseRequested)
    }

    fn send(&mut self, command: HostCommand) {
        if let Some(session) = &self.session
            && let Err(error) = session.send(command)
        {
            self.connection = ConnectionState::Failed;
            self.detail = error;
        }
    }

    fn handle_hook_event(&mut self, event: HookEvent) {
        match event {
            HookEvent::CaptureStarted => {
                self.connection = ConnectionState::Streaming;
                self.detail = "歌词同步已就绪，正在等待 WeSing 加载歌词…".into();
            }
            HookEvent::CaptureStopped => {
                self.connection = ConnectionState::Connected;
                self.detail = "歌词读取已停止。".into();
            }
            HookEvent::Timeline(timeline) => {
                self.detail = "歌词同步中。".into();
                self.playback = None;
                self.timeline = Some(timeline);
            }
            HookEvent::Playback(playback) => {
                if self
                    .timeline
                    .as_ref()
                    .is_some_and(|timeline| timeline.id == playback.timeline_id)
                {
                    self.playback = Some(playback);
                }
            }
            HookEvent::Warning(message) => self.detail = format!("警告：{message}"),
            HookEvent::Error(message) => {
                self.connection = ConnectionState::Failed;
                self.detail = message;
            }
            HookEvent::Pong { .. } => {}
        }
    }

    fn view(&self, window: window::Id) -> Element<'_, Message> {
        if window == self.control_window {
            self.control_view()
        } else if self.lyrics_window == Some(window) {
            self.lyrics_window_view()
        } else {
            container(text("")).into()
        }
    }

    fn control_view(&self) -> Element<'_, Message> {
        let status = match self.connection {
            ConnectionState::Disconnected => "未连接",
            ConnectionState::Connecting => "连接中",
            ConnectionState::Connected => "已连接",
            ConnectionState::Streaming => "歌词同步中",
            ConnectionState::Failed => "错误",
        };

        let executable = text_input("WeSing.exe 路径", &self.executable_path)
            .on_input(Message::ExecutablePathChanged)
            .width(Fill);
        let browse = button("浏览…").on_press(Message::BrowseExecutable);
        let can_launch = matches!(
            self.connection,
            ConnectionState::Disconnected | ConnectionState::Failed
        ) && !self.executable_path.trim().is_empty();
        let launch = button("启动 WeSing").on_press_maybe(can_launch.then_some(Message::Launch));
        let disconnect = button("断开").on_press_maybe(
            matches!(
                self.connection,
                ConnectionState::Connected | ConnectionState::Streaming
            )
            .then_some(Message::Disconnect),
        );
        let lyrics_window = button(if self.lyrics_window.is_some() {
            "显示歌词窗口"
        } else {
            "重新打开歌词窗口"
        })
        .on_press(Message::ShowLyricsWindow);
        let background_color = text_input("#RRGGBB", &self.lyrics_appearance.background_input)
            .on_input(Message::BackgroundColorChanged)
            .width(150);
        let text_color = text_input("#RRGGBB", &self.lyrics_appearance.text_input)
            .on_input(Message::TextColorChanged)
            .width(150);
        let highlight_color = text_input("#RRGGBB", &self.lyrics_appearance.highlight_input)
            .on_input(Message::HighlightColorChanged)
            .width(150);
        let background_preview = color_swatch(self.lyrics_appearance.background);
        let text_preview = color_swatch(self.lyrics_appearance.text);
        let highlight_preview = color_swatch(self.lyrics_appearance.highlight);
        let font = pick_list(
            self.available_fonts.as_slice(),
            Some(self.lyrics_appearance.font),
            Message::LyricsFontChanged,
        )
        .width(200);
        let alignment = pick_list(
            LyricsAlignment::ALL,
            Some(self.lyrics_appearance.alignment),
            Message::LyricsAlignmentChanged,
        )
        .width(120);
        let active_font_size = slider(
            20.0..=96.0,
            self.lyrics_appearance.active_font_size,
            Message::ActiveFontSizeChanged,
        )
        .step(1.0_f32)
        .width(Fill);
        let candidate_font_size = slider(
            12.0..=72.0,
            self.lyrics_appearance.candidate_font_size,
            Message::CandidateFontSizeChanged,
        )
        .step(1.0_f32)
        .width(Fill);
        let show_previous_line = checkbox(self.lyrics_appearance.show_previous_line)
            .label("显示上一句歌词")
            .on_toggle(Message::ShowPreviousLineChanged);
        let candidate_line_count = slider(
            0.0..=10.0,
            self.lyrics_appearance.candidate_line_count as f32,
            Message::CandidateLineCountChanged,
        )
        .step(1.0_f32)
        .width(Fill);
        let colors_valid = parse_hex_color(&self.lyrics_appearance.background_input).is_some()
            && parse_hex_color(&self.lyrics_appearance.text_input).is_some()
            && parse_hex_color(&self.lyrics_appearance.highlight_input).is_some();

        let content = column![
            text("KG Capture").size(32),
            text(format!("状态：{status}")),
            text(&self.detail),
            row![executable, browse].spacing(8),
            row![launch, disconnect].spacing(12),
            lyrics_window,
            text("歌词窗口样式").size(20),
            row![
                text("背景色").width(72),
                background_color,
                background_preview,
                text("文本色").width(72),
                text_color,
                text_preview,
            ]
            .spacing(10)
            .align_y(iced::Center),
            row![text("高亮色").width(72), highlight_color, highlight_preview,]
                .spacing(10)
                .align_y(iced::Center),
            row![
                text("字体").width(72),
                font,
                text("对齐").width(52),
                alignment,
            ]
            .spacing(10)
            .align_y(iced::Center),
            row![
                text("播放中字号").width(92),
                active_font_size,
                text(format!("{:.0} px", self.lyrics_appearance.active_font_size)).width(58),
            ]
            .spacing(10)
            .align_y(iced::Center),
            row![
                text("候选字号").width(92),
                candidate_font_size,
                text(format!(
                    "{:.0} px",
                    self.lyrics_appearance.candidate_font_size
                ))
                .width(58),
            ]
            .spacing(10)
            .align_y(iced::Center),
            row![
                show_previous_line,
                text("候选条目数").width(92),
                candidate_line_count,
                text(format!(
                    "{} 条",
                    self.lyrics_appearance.candidate_line_count
                ))
                .width(58),
            ]
            .spacing(10)
            .align_y(iced::Center),
            text(if colors_valid {
                "颜色使用 #RRGGBB 格式，修改会实时应用到歌词窗口。"
            } else {
                "颜色格式无效；请输入类似 #1A1A1A 的六位十六进制颜色。"
            })
            .size(13),
        ]
        .spacing(12)
        .padding(24);

        container(content).width(Fill).height(Fill).into()
    }

    fn lyrics_window_view(&self) -> Element<'_, Message> {
        let alignment = self.lyrics_appearance.alignment.horizontal();
        let lyrics = match (&self.timeline, &self.playback) {
            (Some(timeline), Some(playback)) => {
                lyric_view(timeline, playback, &self.lyrics_appearance)
            }
            (Some(_), None) => container(
                text("等待播放位置…")
                    .font(self.lyrics_appearance.font.font())
                    .size(self.lyrics_appearance.candidate_font_size)
                    .color(self.lyrics_appearance.text),
            )
            .width(Fill)
            .height(Fill)
            .align_x(alignment)
            .center_y(Fill)
            .into(),
            _ => container(
                text("等待歌词…")
                    .font(self.lyrics_appearance.font.font())
                    .size(self.lyrics_appearance.candidate_font_size)
                    .color(self.lyrics_appearance.text),
            )
            .width(Fill)
            .height(Fill)
            .align_x(alignment)
            .center_y(Fill)
            .into(),
        };

        let background = self.lyrics_appearance.background;
        container(lyrics)
            .width(Fill)
            .height(Fill)
            .padding(36)
            .style(move |_| container::Style::default().background(background))
            .into()
    }
}

fn open_lyrics_window() -> (window::Id, Task<window::Id>) {
    window::open(window::Settings {
        size: Size::new(900.0, 420.0),
        min_size: Some(Size::new(480.0, 240.0)),
        position: window::Position::Centered,
        exit_on_close_request: false,
        ..window::Settings::default()
    })
}

fn lyric_view<'a>(
    timeline: &'a LyricTimeline,
    playback: &'a PlaybackPosition,
    appearance: &LyricsAppearance,
) -> Element<'a, Message> {
    let current_index = playback
        .current_line
        .and_then(|index| usize::try_from(index).ok())
        .filter(|index| *index < timeline.lines.len());
    let progress = playback.line_progress.clamp(0.0, 1.0);
    let mut body = column![].spacing(14).width(Fill);

    if let Some(index) = current_index {
        if appearance.show_previous_line && index > 0 {
            body = body.push(
                text(&timeline.lines[index - 1].text)
                    .font(appearance.font.font())
                    .size(appearance.candidate_font_size)
                    .width(Fill)
                    .align_x(appearance.alignment.horizontal())
                    .color(dim_color(appearance.text, 0.55)),
            );
        }
        body = body.push(current_line_view(
            &timeline.lines[index],
            playback.position_ms,
            progress,
            appearance,
        ));
        for line in timeline
            .lines
            .iter()
            .skip(index + 1)
            .take(appearance.candidate_line_count)
        {
            body = body.push(
                text(&line.text)
                    .font(appearance.font.font())
                    .size(appearance.candidate_font_size)
                    .width(Fill)
                    .align_x(appearance.alignment.horizontal())
                    .color(dim_color(appearance.text, 0.78)),
            );
        }
    } else {
        body = body.push(
            text("等待第一句歌词…")
                .font(appearance.font.font())
                .size(appearance.candidate_font_size)
                .width(Fill)
                .align_x(appearance.alignment.horizontal())
                .color(appearance.text),
        );
        for line in timeline.lines.iter().take(appearance.candidate_line_count) {
            body = body.push(
                text(&line.text)
                    .font(appearance.font.font())
                    .size(appearance.candidate_font_size)
                    .width(Fill)
                    .align_x(appearance.alignment.horizontal())
                    .color(dim_color(appearance.text, 0.78)),
            );
        }
    }

    container(body)
        .width(Fill)
        .height(Fill)
        .center_y(Fill)
        .into()
}

fn current_line_view<'a>(
    line: &'a LyricLine,
    position_ms: f32,
    line_progress: f32,
    appearance: &LyricsAppearance,
) -> Element<'a, Message> {
    if line.words.is_empty() {
        return text(&line.text)
            .font(appearance.font.font())
            .size(appearance.active_font_size)
            .width(Fill)
            .align_x(appearance.alignment.horizontal())
            .color(progress_color(
                appearance.text,
                appearance.highlight,
                line_progress,
            ))
            .into();
    }

    let mut words = row![].spacing(0);
    for word in &line.words {
        let progress = if word.duration_ms > 0.0 {
            ((position_ms - word.start_ms) / word.duration_ms).clamp(0.0, 1.0)
        } else if position_ms >= word.start_ms {
            1.0
        } else {
            0.0
        };
        words = words.push(
            text(&word.text)
                .font(appearance.font.font())
                .size(appearance.active_font_size)
                .color(progress_color(
                    appearance.text,
                    appearance.highlight,
                    progress,
                )),
        );
    }
    container(words.wrap())
        .width(Fill)
        .align_x(appearance.alignment.horizontal())
        .into()
}

fn progress_color(text: Color, highlight: Color, progress: f32) -> Color {
    let progress = progress.clamp(0.0, 1.0);
    let text = dim_color(text, 0.68);
    Color {
        r: text.r + (highlight.r - text.r) * progress,
        g: text.g + (highlight.g - text.g) * progress,
        b: text.b + (highlight.b - text.b) * progress,
        a: text.a + (highlight.a - text.a) * progress,
    }
}

fn parse_hex_color(input: &str) -> Option<Color> {
    let hex = input.trim().strip_prefix('#').unwrap_or(input.trim());
    if hex.len() != 6 || !hex.is_ascii() {
        return None;
    }

    let red = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let green = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let blue = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some(Color::from_rgb8(red, green, blue))
}

fn color_swatch(color: Color) -> Element<'static, Message> {
    container(text(""))
        .width(30)
        .height(30)
        .style(move |_| {
            container::Style::default().background(color).border(
                iced::border::rounded(4)
                    .color(Color::from_rgb8(120, 124, 132))
                    .width(1),
            )
        })
        .into()
}

fn dim_color(color: Color, factor: f32) -> Color {
    Color {
        r: color.r * factor,
        g: color.g * factor,
        b: color.b * factor,
        a: color.a,
    }
}

fn event_stream(
    receiver: Arc<Mutex<IpcReceiver<HookEvent>>>,
) -> impl iced::futures::Stream<Item = Result<HookEvent, String>> {
    iced::stream::channel(16, async move |mut output| {
        let _ = std::thread::Builder::new()
            .name("kg-capture-host-events".into())
            .spawn(move || {
                loop {
                    let event = receiver
                        .lock()
                        .map_err(|_| "hook event receiver lock was poisoned".to_owned())
                        .and_then(|receiver| {
                            receiver
                                .recv()
                                .map_err(|error| format!("hook disconnected: {error}"))
                        });
                    let disconnected = event.is_err();
                    if iced::futures::executor::block_on(output.send(event)).is_err()
                        || disconnected
                    {
                        break;
                    }
                }
            });
        std::future::pending::<()>().await;
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_six_digit_hex_colors() {
        assert_eq!(
            parse_hex_color("#1a2B3c"),
            Some(Color::from_rgb8(0x1a, 0x2b, 0x3c))
        );
        assert_eq!(
            parse_hex_color(" FFFFFF "),
            Some(Color::from_rgb8(0xff, 0xff, 0xff))
        );
    }

    #[test]
    fn rejects_invalid_hex_colors() {
        assert_eq!(parse_hex_color("#123"), None);
        assert_eq!(parse_hex_color("#GG0000"), None);
        assert_eq!(parse_hex_color("#12345678"), None);
    }
}
