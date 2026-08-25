use freya::prelude::*;

use crate::appearance::Rgb;
use crate::colors::{
    app_bg, border, border_focus, card_bg, check_mark, control_bg, hover_bg, panel_bg, selected_bg,
    text_fg,
};

fn rgb(color: Rgb) -> Color {
    Color::from_rgb(color.0, color.1, color.2)
}

pub fn lege_panel_card(
    title: impl Into<String>,
    fill_height: bool,
    children: Vec<Element>,
) -> Element {
    let title = title.into();
    let mut card = rect()
        .background(card_bg())
        .border(
            Border::new()
                .fill(border())
                .width(1.)
                .alignment(BorderAlignment::Inner),
        )
        .corner_radius(3.)
        .padding(7.)
        .width(Size::fill())
        .content(Content::Flex)
        .spacing(4.0_f32);

    if fill_height {
        card = card.height(Size::fill());
    }

    if !title.is_empty() {
        card = card.child(
            rect().width(Size::fill()).height(Size::px(22.0_f32)).child(
                label()
                    .text(title)
                    .font_size(15.)
                    .color(text_fg())
                    .font_weight(700),
            ),
        );
    }

    card.children(children).into()
}

pub fn lege_field(label_text: impl Into<String>, control: Element) -> Element {
    rect()
        .vertical()
        .width(Size::fill())
        .spacing(1.0_f32)
        .child(
            label()
                .text(label_text.into())
                .font_size(13.)
                .color(text_fg())
                .font_weight(500),
        )
        .child(
            rect()
                .width(Size::fill())
                .min_height(Size::px(24.0_f32))
                .child(control),
        )
        .into()
}

pub fn lege_grid_row(children: Vec<Element>, height: f32, spacing: f32) -> Element {
    let cells: Vec<Element> = children
        .into_iter()
        .map(|child| {
            rect()
                .width(Size::flex(1.0_f32))
                .height(Size::fill())
                .child(child)
                .into()
        })
        .collect();

    rect()
        .width(Size::fill())
        .height(Size::px(height))
        .direction(Direction::Horizontal)
        .content(Content::Flex)
        .spacing(spacing)
        .children(cells)
        .into()
}

#[derive(Clone, PartialEq)]
struct LegeCheckboxRow {
    text: String,
    selected: bool,
    on_select: EventHandler<()>,
}

impl LegeCheckboxRow {
    fn new(
        text: impl Into<String>,
        selected: bool,
        on_select: impl Into<EventHandler<()>>,
    ) -> Self {
        Self {
            text: text.into(),
            selected,
            on_select: on_select.into(),
        }
    }
}

impl Component for LegeCheckboxRow {
    fn render(&self) -> impl IntoElement {
        let mut hovering = use_state(|| false);
        let selected = self.selected;
        let on_select = self.on_select.clone();
        let text = self.text.clone();

        let box_background = if selected {
            rgb(selected_bg())
        } else if hovering() {
            rgb(hover_bg())
        } else {
            rgb(control_bg())
        };

        rect()
            .direction(Direction::Horizontal)
            .padding((0., 2., 0., 2.))
            .spacing(8.0_f32)
            .cross_align(Alignment::Center)
            .on_pointer_enter(move |_| {
                hovering.set(true);
                Cursor::set(CursorIcon::Pointer);
            })
            .on_pointer_leave(move |_| {
                hovering.set(false);
                Cursor::set(CursorIcon::default());
            })
            .on_press(move |e: Event<PressEventData>| {
                e.stop_propagation();
                on_select.call(());
            })
            .child(
                rect()
                    .width(Size::px(14.0_f32))
                    .height(Size::px(14.0_f32))
                    .border(
                        Border::new()
                            .fill(rgb(border_focus()))
                            .width(1.)
                            .alignment(BorderAlignment::Inner),
                    )
                    .background(box_background)
                    .main_align(Alignment::Center)
                    .cross_align(Alignment::Center)
                    .maybe_child(if selected {
                        Some(
                            label()
                                .text("X")
                                .font_size(11.)
                                .font_weight(700)
                                .color(rgb(check_mark()))
                                .into(),
                        )
                    } else {
                        None::<Element>
                    }),
            )
            .child(label().text(text).font_size(13.).color(text_fg()))
    }
}

pub fn lege_checkbox_row(
    text: impl Into<String>,
    selected: bool,
    on_select: impl FnMut(()) + 'static,
) -> Element {
    // Freya's built-in `Tile` has a fixed padding of 8px, which makes rows feel too tall here.
    // This custom row keeps the same behavior but with tighter spacing.
    LegeCheckboxRow::new(text, selected, on_select).into()
}

pub fn lege_file_action_row(
    add_file: Element,
    add_folder: Element,
    output_directory: Element,
) -> Element {
    rect()
        .width(Size::fill())
        .height(Size::fill())
        .direction(Direction::Horizontal)
        .main_align(Alignment::Center)
        .cross_align(Alignment::Center)
        .spacing(4.0_f32)
        .content(Content::Flex)
        .child(
            rect()
                .width(Size::px(132.0_f32))
                .height(Size::px(40.0_f32))
                .child(add_file),
        )
        .child(
            rect()
                .width(Size::px(132.0_f32))
                .height(Size::px(40.0_f32))
                .child(add_folder),
        )
        .child(
            rect()
                .width(Size::px(270.0_f32))
                .height(Size::px(40.0_f32))
                .child(output_directory),
        )
        .into()
}

pub fn lege_status_panel(
    status_content: Element,
    top_left: Element,
    top_right: Element,
    bottom_left: Element,
    bottom_right: Element,
) -> Element {
    rect()
        .background(card_bg())
        .border(
            Border::new()
                .fill(border())
                .width(1.)
                .alignment(BorderAlignment::Inner),
        )
        .corner_radius(6.)
        .padding(5.)
        .width(Size::fill())
        .height(Size::fill())
        .child(
            rect()
                .width(Size::fill())
                .height(Size::fill())
                .padding((8., 8., 8., 8.))
                .child(status_content),
        )
        .child(
            rect()
                .position(Position::new_absolute().left(8.).top(8.))
                .child(top_left),
        )
        .child(
            rect()
                .position(Position::new_absolute().right(8.).top(14.))
                .child(top_right),
        )
        .child(
            // Notification popups must paint above the status content (e.g. the PDF
            // title), which is nested more deeply and would otherwise get a higher
            // absolute layer. A moderate relative boost lifts the whole popup subtree
            // above the status content while staying well below modal Overlay layers,
            // so real dialogs still render on top of these notifications.
            rect()
                .layer(50i16)
                .position(Position::new_absolute().left(8.).bottom(8.))
                .child(bottom_left),
        )
        .child(
            rect()
                .position(Position::new_absolute().right(8.).bottom(8.))
                .child(bottom_right),
        )
        .into()
}

pub fn lege_main_shell(
    file_actions: Element,
    settings: Element,
    process_row: Element,
    status_bar: Element,
) -> Element {
    rect()
        .background(app_bg())
        .width(Size::fill())
        .height(Size::fill())
        .padding(6.)
        .child(
            rect()
                .background(panel_bg())
                .border(
                    Border::new()
                        .fill(border())
                        .width(1.)
                        .alignment(BorderAlignment::Inner),
                )
                .corner_radius(8.)
                .width(Size::fill())
                .height(Size::fill())
                .child(
                    rect()
                        .padding(7.)
                        .spacing(6.0_f32)
                        .width(Size::fill())
                        .height(Size::fill())
                        .vertical()
                        .content(Content::Flex)
                        .child(
                            rect()
                                .width(Size::fill())
                                .height(Size::px(56.0_f32))
                                .child(file_actions),
                        )
                        .child(
                            // Sheet Music Edition: the settings dashboard is a
                            // single compact card — a 48px control strip (page
                            // range / resolution / format; OCR removed) plus the
                            // binarization mode row (adaptive / fixed / heavy),
                            // including its 15px clearance below the controls.
                            // Still far below the main edition's 300px.
                            rect()
                                .width(Size::fill())
                                .height(Size::px(171.0_f32))
                                .child(settings),
                        )
                        .child(
                            rect()
                                .width(Size::fill())
                                .height(Size::px(126.0_f32))
                                .child(process_row),
                        )
                        .child(
                            // Status/queue area, ~60% shorter than before so it no
                            // longer absorbs the freed flex space as dead height. The
                            // 132px floor still clears the tallest thing drawn inside
                            // it: the processing progress display (~117px) and the
                            // idle hover tooltips (250px-wide, up to ~126px tall).
                            rect()
                                .width(Size::fill())
                                .height(Size::flex(1.0_f32))
                                .min_height(Size::px(132.0_f32))
                                .child(status_bar),
                        ),
                ),
        )
        .into()
}
