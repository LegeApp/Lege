use freya_animation::prelude::*;
use freya_core::prelude::*;
use torin::{alignment::Alignment, direction::Direction, prelude::Position, size::Size};

use crate::{button::Button, popup::PopupBackground};

#[derive(Clone, PartialEq)]
pub struct LegeModal {
    title: Readable<String>,
    content: Element,
    show: Readable<bool>,
    on_close_request: Option<EventHandler<()>>,
    close_on_escape_key: bool,
    width: Size,
    height: Size,
    min_width: Size,
    min_height: Size,
    key: DiffKey,
}

impl KeyExt for LegeModal {
    fn write_key(&mut self) -> &mut DiffKey {
        &mut self.key
    }
}

impl LegeModal {
    pub fn new(title: impl Into<Readable<String>>, content: Element) -> Self {
        Self {
            title: title.into(),
            content,
            show: true.into(),
            on_close_request: None,
            close_on_escape_key: true,
            width: Size::percent(50.),
            height: Size::percent(50.),
            min_width: Size::px(520.),
            min_height: Size::px(360.),
            key: DiffKey::None,
        }
    }

    pub fn show(mut self, show: impl Into<Readable<bool>>) -> Self {
        self.show = show.into();
        self
    }

    pub fn on_close_request(mut self, on_close_request: impl Into<EventHandler<()>>) -> Self {
        self.on_close_request = Some(on_close_request.into());
        self
    }

    pub fn width(mut self, width: impl Into<Size>) -> Self {
        self.width = width.into();
        self
    }

    pub fn height(mut self, height: impl Into<Size>) -> Self {
        self.height = height.into();
        self
    }

    pub fn min_width(mut self, min_width: impl Into<Size>) -> Self {
        self.min_width = min_width.into();
        self
    }

    pub fn min_height(mut self, min_height: impl Into<Size>) -> Self {
        self.min_height = min_height.into();
        self
    }
}

impl Component for LegeModal {
    fn render(&self) -> impl IntoElement {
        let show = *self.show.read();

        let background_animation = use_animation_with_dependencies(&show, |conf, show| {
            conf.on_creation(OnCreation::Finish);
            conf.on_change(OnChange::Rerun);

            let value = AnimColor::new((0, 0, 0, 0), (0, 0, 0, 135)).time(150);

            if *show { value } else { value.into_reversed() }
        });

        let content_animation = use_animation_with_dependencies(&show, |conf, _| {
            conf.on_creation(OnCreation::Finish);
            conf.on_change(OnChange::Rerun);

            (
                AnimNum::new(0.9, 1.)
                    .time(180)
                    .ease(Ease::Out)
                    .function(Function::Expo),
                AnimNum::new(0.15, 1.)
                    .time(180)
                    .ease(Ease::Out)
                    .function(Function::Expo),
            )
        });

        let should_render = show || *background_animation.is_running().read();

        let request_to_close = {
            let handler = self.on_close_request.clone();
            move || {
                if let Some(h) = &handler {
                    h.call(());
                }
            }
        };

        let on_global_key_down = {
            let close = self.close_on_escape_key;
            let req = request_to_close.clone();
            move |e: Event<KeyboardEventData>| {
                if close && e.key == Key::Named(NamedKey::Escape) {
                    req();
                }
            }
        };

        rect()
            .layer(Layer::Overlay)
            .position(Position::new_global())
            .maybe_child(should_render.then(|| {
                let background_color = background_animation.get().value();
                let (scale_anim, opacity_anim) = &*content_animation.read();
                let (scale, opacity) = if show {
                    (scale_anim.value(), opacity_anim.value())
                } else {
                    (1., 0.)
                };

                let request_to_close_button = request_to_close.clone();
                let request_to_close_overlay = request_to_close.clone();

                PopupBackground::new(
                    rect()
                        .a11y_role(AccessibilityRole::Dialog)
                        .scale((scale, scale))
                        .opacity(opacity)
                        .background((240, 244, 247))
                        .color((18, 18, 18))
                        .corner_radius(10.)
                        .shadow(Shadow::new().y(4.).blur(8.).color((0, 0, 0, 40)))
                        .width(self.width.clone())
                        .height(self.height.clone())
                        .min_width(self.min_width.clone())
                        .min_height(self.min_height.clone())
                        .spacing(0.)
                        .on_global_key_down(on_global_key_down)
                        .child(
                            rect()
                                .width(Size::fill())
                                .height(Size::px(42.))
                                .padding((10., 14., 10., 14.))
                                .background((226, 232, 235))
                                .direction(Direction::Horizontal)
                                .main_align(Alignment::SpaceBetween)
                                .cross_align(Alignment::Center)
                                .child(
                                    label()
                                        .text(self.title.read().to_string())
                                        .font_size(14.)
                                        .font_weight(700),
                                )
                                .child(
                                    Button::new()
                                        .compact()
                                        .on_press(move |_| request_to_close_button())
                                        .child("Close"),
                                ),
                        )
                        .child(rect().expanded().padding(12.).child(self.content.clone()))
                        .into(),
                    move |_| request_to_close_overlay(),
                    background_color,
                )
            }))
    }

    fn render_key(&self) -> DiffKey {
        self.key.clone().or(self.default_key())
    }
}
