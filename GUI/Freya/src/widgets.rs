use freya::prelude::*;

const APP_BG: (u8, u8, u8) = (226, 232, 235);
const PANEL_BG: (u8, u8, u8) = (255, 250, 250);
const CARD_BG: (u8, u8, u8) = (255, 250, 250);
const TEXT: (u8, u8, u8) = (20, 20, 20);
const MUTED: (u8, u8, u8) = (90, 90, 90);
const BORDER: (u8, u8, u8) = (128, 128, 128);

pub fn lege_panel_card(title: impl Into<String>, fill_height: bool, children: Vec<Element>) -> Element {
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
            label()
                .text(title)
                .font_size(15.)
                .color(TEXT)
                .font_weight(700),
        );
    }

    card.children(children).into()
}

pub fn lege_section_label(text: impl Into<String>) -> Element {
    label()
        .text(text.into())
        .font_size(13.5)
        .color(MUTED)
        .font_weight(700)
        .into()
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

pub fn lege_toggle_row(
    label_text: impl Into<String>,
    current_text: impl Into<String>,
    on_press: impl FnMut(Event<PressEventData>) + 'static,
) -> Element {
    lege_field(
        label_text,
        Button::new()
            .width(Size::px(104.))
            .height(Size::px(24.))
            .on_press(on_press)
            .child(current_text.into())
            .into(),
    )
}

pub fn lege_inline_toggle_row(
    label_text: impl Into<String>,
    current_text: impl Into<String>,
    on_press: impl FnMut(Event<PressEventData>) + 'static,
) -> Element {
    let label_text = label_text.into();
    let current_text = current_text.into();

    rect()
        .width(Size::fill())
        .height(Size::px(28.))
        .padding((0., 0., 4., 0.))
        .direction(Direction::Horizontal)
        .cross_align(Alignment::Center)
        .spacing(8.)
        .child(
            rect()
                .width(Size::px(150.))
                .height(Size::fill())
                .cross_align(Alignment::Start)
                .main_align(Alignment::Center)
                .child(
                    label()
                        .text(label_text)
                        .font_size(13.)
                        .color(TEXT)
                        .font_weight(500),
                ),
        )
        .child(
            Button::new()
                .width(Size::px(104.))
                .height(Size::px(24.))
                .on_press(on_press)
                .child(current_text),
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

pub fn lege_toolbar(
    add_pdf: Element,
    add_folder: Element,
    output_directory: Element,
    utility_column: Element,
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
        .height(Size::px(56.))
        .direction(Direction::Horizontal)
        .cross_align(Alignment::Center)
        .spacing(6.)
        .child(
            rect()
                .width(Size::percent(29.))
                .height(Size::px(38.))
                .child(add_pdf),
        )
        .child(
            rect()
                .width(Size::percent(29.))
                .height(Size::px(38.))
                .child(add_folder),
        )
        .child(
            rect()
                .width(Size::percent(29.))
                .height(Size::px(38.))
                .child(output_directory),
        )
        .child(
            rect()
                .width(Size::px(90.))
                .height(Size::px(38.))
                .child(utility_column),
        )
        .into()
}

pub fn lege_settings_grid(left_panel: Element, right_panel: Element) -> Element {
    rect()
        .width(Size::fill())
        .height(Size::fill())
        .direction(Direction::Horizontal)
        .cross_align(Alignment::Start)
        .spacing(8.)
        .child(
            rect()
                .width(Size::percent(50.))
                .height(Size::fill())
                .child(left_panel),
        )
        .child(
            rect()
                .width(Size::percent(50.))
                .height(Size::fill())
                .child(right_panel),
        )
        .into()
}

pub fn lege_status_panel(status_content: Element, action_content: Element) -> Element {
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
        .main_align(Alignment::SpaceBetween)
        .cross_align(Alignment::Center)
        .child(
            rect()
                .expanded()
                .height(Size::fill())
                .main_align(Alignment::Center)
                .child(status_content),
        )
        .child(
            rect()
                .width(Size::px(372.))
                .height(Size::fill())
                .child(action_content),
        )
        .into()
}

pub fn lege_main_shell(
    toolbar: Element,
    settings: Element,
    popup_rail: Element,
    process_bar: Element,
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
                        .spacing(6.)
                        .width(Size::fill())
                        .height(Size::fill())
                        .vertical()
                        .child(toolbar)
                        .child(
                            rect()
                                .width(Size::fill())
                                .height(Size::px(336.))
                                .child(settings),
                        )
                        .child(
                            rect()
                                .expanded()
                                .width(Size::fill())
                                .vertical()
                                .spacing(4.)
                                .child(
                                    rect()
                                        .width(Size::fill())
                                        .height(Size::percent(40.))
                                        .child(
                                            rect()
                                                .width(Size::fill())
                                                .height(Size::fill())
                                                .child(
                                                    rect()
                                                        .width(Size::fill())
                                                        .height(Size::fill())
                                                        .cross_align(Alignment::Center)
                                                        .main_align(Alignment::End)
                                                        .child(process_bar),
                                                )
                                                .child(
                                                    rect()
                                                        .width(Size::px(220.))
                                                        .height(Size::fill())
                                                        .position(Position::new_absolute().left(0.).top(0.))
                                                        .child(popup_rail),
                                                ),
                                        )
                                )
                                .child(
                                    rect()
                                        .width(Size::fill())
                                        .height(Size::percent(60.))
                                        .min_height(Size::px(156.))
                                        .child(status_bar),
                                ),
                        ),
                ),
        )
        .into()
}
