use freya::prelude::*;

use crate::colors::{APP_BG, BORDER, CARD_BG, MUTED, PANEL_BG, TEXT};

pub fn lege_panel_card(
    title: impl Into<String>,
    fill_height: bool,
    children: Vec<Element>,
) -> Element {
    let title = title.into();
    let mut card = rect()
        .background(CARD_BG)
        .border(
            Border::new()
                .fill(BORDER)
                .width(1.)
                .alignment(BorderAlignment::Inner),
        )
        .corner_radius(3.)
        .padding(7.)
        .width(Size::fill())
        .content(Content::Flex)
        .spacing(4.);

    if fill_height {
        card = card.height(Size::fill());
    }

    if !title.is_empty() {
        card = card.child(
            rect()
                .width(Size::fill())
                .height(Size::px(22.))
                .child(
                    label()
                        .text(title)
                        .font_size(15.)
                        .color(TEXT)
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
        .spacing(1.)
        .child(
            label()
                .text(label_text.into())
                .font_size(13.)
                .color(TEXT)
                .font_weight(500),
        )
        .child(
            rect()
                .width(Size::fill())
                .min_height(Size::px(24.))
                .child(control),
        )
        .into()
}

pub fn lege_checkbox_row(
    text: impl Into<String>,
    selected: bool,
    mut on_select: impl FnMut(()) + 'static,
) -> Element {
    // Freya's built-in `Tile` has a fixed padding of 8px, which makes rows feel too tall here.
    // This custom row keeps the same behavior but with tighter spacing.
    rect()
        .direction(Direction::Horizontal)
        .padding((0., 2., 0., 2.))
        .spacing(8.)
        .cross_align(Alignment::Center)
        .on_press(move |_| on_select(()))
        .child(
            rect()
                .width(Size::px(14.))
                .height(Size::px(14.))
                .border(
                    Border::new()
                        .fill(Color::from_rgb(64, 64, 64))
                        .width(1.)
                        .alignment(BorderAlignment::Inner),
                )
                .background(if selected {
                    Color::from_rgb(205, 205, 205)
                } else {
                    Color::from_rgb(255, 250, 250)
                })
                .main_align(Alignment::Center)
                .cross_align(Alignment::Center)
                .maybe_child(if selected {
                    Some(
                        label()
                            .text("X")
                            .font_size(11.)
                            .font_weight(700)
                            .color(Color::from_rgb(30, 30, 30))
                            .into(),
                    )
                } else {
                    None::<Element>
                }),
        )
        .child(label().text(text.into()).font_size(13.).color(TEXT))
        .into()
}

pub fn lege_header_bar(left: Element, utilities: Element) -> Element {
    rect()
        .width(Size::fill())
        .height(Size::fill())
        .direction(Direction::Horizontal)
        .main_align(Alignment::SpaceBetween)
        .cross_align(Alignment::Center)
        .spacing(8.)
        .child(left)
        .child(utilities)
        .into()
}

pub fn lege_file_action_row(
    add_file: Element,
    add_folder: Element,
    output_directory: Element,
) -> Element {
    rect()
        .background(CARD_BG)
        .border(
            Border::new()
                .fill(BORDER)
                .width(1.)
                .alignment(BorderAlignment::Inner),
        )
        .corner_radius(6.)
        .padding(5.)
        .width(Size::fill())
        .height(Size::fill())
        .direction(Direction::Horizontal)
        .cross_align(Alignment::Center)
        .spacing(6.)
        .content(Content::Flex)
        .child(
            rect()
                .width(Size::flex(1.))
                .height(Size::px(40.))
                .child(add_file),
        )
        .child(
            rect()
                .width(Size::flex(1.))
                .height(Size::px(40.))
                .child(add_folder),
        )
        .child(
            rect()
                .width(Size::flex(1.))
                .height(Size::px(40.))
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
        .background(CARD_BG)
        .border(
            Border::new()
                .fill(BORDER)
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
    header: Element,
    file_actions: Element,
    settings: Element,
    process_row: Element,
    status_bar: Element,
) -> Element {
    rect()
        .background(APP_BG)
        .width(Size::fill())
        .height(Size::fill())
        .padding(6.)
        .child(
            rect()
                .background(PANEL_BG)
                .border(
                    Border::new()
                        .fill(BORDER)
                        .width(1.)
                        .alignment(BorderAlignment::Inner),
                )
                .corner_radius(8.)
                .width(Size::fill())
                .height(Size::fill())
                .child(
                    rect()
                        .padding(8.)
                        .spacing(8.)
                        .width(Size::fill())
                        .height(Size::fill())
                        .vertical()
                        .content(Content::Flex)
                        .child(
                            rect()
                                .width(Size::fill())
                                .height(Size::px(34.))
                                .child(header),
                        )
                        .child(
                            rect()
                                .width(Size::fill())
                                .height(Size::px(56.))
                                .child(file_actions),
                        )
                        .child(
                            rect()
                                .width(Size::fill())
                                .height(Size::px(326.))
                                .child(settings),
                        )
                        .child(
                            rect()
                                .width(Size::fill())
                                .height(Size::px(52.))
                                .child(process_row),
                        )
                        .child(
                            rect()
                                .width(Size::fill())
                                .height(Size::flex(1.))
                                .min_height(Size::px(170.))
                                .child(status_bar),
                        ),
                ),
        )
        .into()
}
