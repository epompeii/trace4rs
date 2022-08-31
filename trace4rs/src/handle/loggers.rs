#![allow(clippy::single_char_lifetime_names)]
use core::fmt;
use std::{
    borrow::Cow,
    io,
};

use fmtorp::Fmtr;
use once_cell::sync::Lazy;
use tracing::{
    field::Visit,
    metadata::LevelFilter,
    Event,
    Metadata,
};
use tracing_log::NormalizeEvent;
use tracing_subscriber::{
    fmt::{
        format::{
            DefaultFields,
            Format,
            Full,
            Writer,
        },
        time::FormatTime,
        writer::{
            BoxMakeWriter,
            MakeWriterExt,
        },
        FmtContext,
        FormatEvent,
        FormatFields,
        Layer as FmtLayer,
    },
    layer::{
        Context,
        Layered,
    },
    prelude::__tracing_subscriber_SubscriberExt,
    Layer,
};

use super::{
    span_broker::SpanBroker,
    PolyLayer,
};
use crate::{
    appenders::Appenders,
    config::{
        AppenderId,
        Format as ConfigFormat,
        Target,
    },
};

static NORMAL_FMT: Lazy<Format<Full, UtcOffsetTime>> =
    Lazy::new(|| Format::default().with_timer(UtcOffsetTime).with_ansi(false));

pub struct Logger<N = DefaultFields, F = EventFormatter> {
    level:  LevelFilter,
    target: Option<Target>,
    layer:  Layered<FmtLayer<SpanBroker, N, F, BoxMakeWriter>, SpanBroker>,
}
impl Logger {
    pub fn new_erased<'a>(
        r: SpanBroker,
        level: LevelFilter,
        target: Option<Target>,
        ids: impl IntoIterator<Item = &'a AppenderId>,
        appenders: &Appenders,
        format: EventFormatter,
    ) -> PolyLayer<SpanBroker> {
        Box::new(Self::new(
            r,
            level,
            target,
            ids.into_iter(),
            appenders,
            format,
        ))
    }

    fn is_enabled(&self, meta: &Metadata<'_>) -> bool {
        let match_level = meta.level() <= &self.level;
        let match_target = self
            .target
            .as_ref()
            .map_or(true, |t| meta.target().starts_with(t.as_str()));

        match_level && match_target
    }

    fn mk_writer<'a>(
        ids: impl Iterator<Item = &'a AppenderId>,
        appenders: &Appenders,
    ) -> Option<BoxMakeWriter> {
        let mut accumulated_makewriter = None;
        for id in ids {
            if let Some(appender) = appenders.get(id).map(ToOwned::to_owned) {
                accumulated_makewriter = if let Some(acc) = accumulated_makewriter.take() {
                    Some(BoxMakeWriter::new(MakeWriterExt::and(acc, appender)))
                } else {
                    Some(BoxMakeWriter::new(appender))
                }
            }
        }
        accumulated_makewriter
    }

    pub fn new<'a>(
        r: SpanBroker,
        level: LevelFilter,
        target: Option<Target>,
        ids: impl Iterator<Item = &'a AppenderId>,
        appenders: &Appenders,
        format: EventFormatter,
    ) -> Self {
        let writer =
            Self::mk_writer(ids, appenders).unwrap_or_else(|| BoxMakeWriter::new(io::sink));

        let fmt_layer = FmtLayer::default().event_format(format).with_ansi(false);
        let append_layer = fmt_layer.with_writer(writer);
        let layer = r.with(append_layer);

        Self {
            level,
            target,
            layer,
        }
    }
}
impl Layer<SpanBroker> for Logger {
    fn enabled(&self, meta: &Metadata<'_>, _ctx: Context<'_, SpanBroker>) -> bool {
        Logger::is_enabled(self, meta)
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, SpanBroker>) {
        self.layer.on_event(event, ctx);
    }
}

pub enum EventFormatter {
    Normal,
    MessageOnly,
    Custom(CustomFormatter),
}

impl From<ConfigFormat> for EventFormatter {
    fn from(f: ConfigFormat) -> Self {
        match f {
            ConfigFormat::Normal => Self::Normal,
            ConfigFormat::MessageOnly => Self::MessageOnly,
            ConfigFormat::Custom(s) => Self::Custom(CustomFormatter::new(s)),
        }
    }
}

impl FormatEvent<SpanBroker, DefaultFields> for EventFormatter {
    fn format_event(
        &self,
        ctx: &tracing_subscriber::fmt::FmtContext<'_, SpanBroker, DefaultFields>,
        writer: tracing_subscriber::fmt::format::Writer<'_>,
        event: &Event<'_>,
    ) -> std::fmt::Result {
        match self {
            Self::Custom(fmtr) => fmtr.format_event(ctx, writer, event),
            Self::MessageOnly => {
                let mut vs = SingleFieldVisitor::new(writer, MESSAGE_FIELD_NAME);
                event.record(&mut vs);
                Ok(())
            },
            Self::Normal => NORMAL_FMT.format_event(ctx, writer, event),
        }
    }
}
mod fields {
    pub const TIMESTAMP: &str = "T";
    pub const TARGET: &str = "t";
    pub const MESSAGE: &str = "m";
    pub const FIELDS: &str = "f";
    pub const LEVEL: &str = "l";
}

struct CustomValueWriter<'ctx, 'evt> {
    fmtr:  Fmtr<'static>,
    ctx:   &'ctx FmtContext<'ctx, SpanBroker, DefaultFields>,
    event: &'evt Event<'evt>,
}
impl<'ctx, 'evt> CustomValueWriter<'ctx, 'evt> {
    fn write(&mut self, mut writer: Writer<'_>) -> fmt::Result {
        self.fmtr.write(&mut writer, self)
    }

    const fn get_field_id(&self, s: &str) -> usize {
        self.fmtr.field_from_name(s)
    }
}
impl<'ctx, 'evt> fmtorp::FieldValueWriter for CustomValueWriter<'ctx, 'evt> {
    fn write_value(&self, writer: &mut impl fmt::Write, field: fmtorp::Field) -> fmt::Result {
        let normalized_meta = self.event.normalized_metadata();
        let meta = normalized_meta
            .as_ref()
            .unwrap_or_else(|| self.event.metadata());

        let id = field.id();

        if id == self.get_field_id(fields::TIMESTAMP) {
            self.format_timestamp(&mut writer)?;
        } else if id == self.get_field_id(fields::TARGET) {
            write!(writer, "{}", meta.target())?;
        } else if id == self.get_field_id(fields::MESSAGE) {
            for f in self.event.fields() {
                if f.name() == MESSAGE_FIELD_NAME {
                    write!(writer, "{}", f.to_string())?;
                }
            }
        } else if id == self.get_field_id(fields::FIELDS) {
            self.ctx.format_fields(writer.by_ref(), self.event)?;
        } else if id == self.get_field_id(fields::LEVEL) {
            write!(writer, "{}", meta.level())?;
        }
        Ok(())
    }
}
/// EAS: Follow strat from NORMAL_FMT
/// move Message only  and this to formatter.rs and utcoffsettime
pub struct CustomFormatter {
    fmtr: fmtorp::Fmtr<'static>,
}
impl CustomFormatter {
    fn new(fmt_str: impl Into<Cow<'static, str>>) -> Self {
        let fmtr = fmtorp::Fmtr::new(fmt_str);

        Self { fmtr }
    }

    fn format_event(
        &self,
        ctx: &FmtContext<'_, SpanBroker, DefaultFields>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let value_writer = CustomValueWriter {
            fmtr: &self.fmtr,
            ctx,
            event,
        };
        value_writer.write(writer)
    }

    #[inline]
    fn format_timestamp(&self, writer: &mut Writer<'_>) -> fmt::Result {
        let t = tracing_subscriber::fmt::time::SystemTime;
        if let Err(_) = t.format_time(writer) {
            writer.write_str("<unknown time>")?;
        }
        Ok(())
    }
}

const MESSAGE_FIELD_NAME: &'static str = "message";

struct SingleFieldVisitor<'w> {
    writer:     tracing_subscriber::fmt::format::Writer<'w>,
    field_name: Cow<'static, str>,
}
impl<'w> SingleFieldVisitor<'w> {
    fn new(
        writer: tracing_subscriber::fmt::format::Writer<'w>,
        field_name: impl Into<Cow<'static, str>>,
    ) -> Self {
        Self {
            writer,
            field_name: field_name.into(),
        }
    }
}
impl<'w> Visit for SingleFieldVisitor<'w> {
    // todo(eas): Might be good to come back to this, looks like this is getting
    // called directly by tracing-subscriber on accident.
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        // eas: bummer to hardcode this but thats how tracing does it
        #[allow(unused_must_use, clippy::use_debug)]
        if field.name() == self.field_name {
            writeln!(self.writer, "{:?}", value);
        }
    }
}

const TIME_FORMAT: time::format_description::well_known::Rfc3339 =
    time::format_description::well_known::Rfc3339;

struct UtcOffsetTime;

impl FormatTime for UtcOffsetTime {
    fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
        let ts =
            time::OffsetDateTime::now_local().unwrap_or_else(|_| time::OffsetDateTime::now_utc());
        let ts_str = ts.format(&TIME_FORMAT).unwrap_or_default();

        w.write_str(&ts_str)
    }
}
