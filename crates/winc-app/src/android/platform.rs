use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use jni::objects::{Global, JObject, JValue};
use jni::{Env, JavaVM, jni_sig, jni_str};
use winc_config::{AutoSave, Config, State};
use winc_editor::Editor;
use winit::event_loop::{EventLoop, EventLoopProxy};
use winit::platform::android::{EventLoopBuilderExtAndroid, activity::AndroidApp};

use crate::app::{App, UserEvent};
use crate::bench::Bench;
use crate::winc::{Envelope, Recipient, Winc};

static PROXY: Mutex<Option<(Arc<Activity>, EventLoopProxy<Envelope>)>> = Mutex::new(None);

/// An Android UI, document provider, or input connection callback.
#[derive(Debug)]
pub enum Event {
    /// Launch the file picker.
    Open,
    /// Save the active Android document.
    Save,
    /// Export the active editor contents to a new document.
    ExportDraft,
    /// Export the last confirmed source bytes to a new document.
    ExportOriginal,
    /// Result of exporting a copy.
    ExportEnded(i64, i32),
    /// Bring up the IME for the active document.
    Keyboard,
    /// Undo the previous editor operation.
    Undo,
    /// A new snapshot of the native input connection, using UTF-16 indices.
    Input {
        /// Connection generation.
        token: i64,
        /// Monotonic snapshot number within the connection.
        sequence: i64,
        /// Selection start and end, then composing start and end.
        positions: [i32; 4],
        /// The editable surrounding text.
        text: String,
    },
    /// Result of refreshing the surrounding text.
    InputRecentered(i64, bool),
    /// The picker produced a private copy and a source URI.
    Opened {
        /// Picker request ID.
        token: i64,
        /// Private copy.
        path: PathBuf,
        /// Storage Access Framework document URI.
        uri: String,
        /// The picker granted write access.
        writable: bool,
    },
    /// The picker was cancelled or the document could not be read.
    OpenEnded(i64, i32),
    /// Result of writing back a staged save.
    Saved(i64, i32),
    /// Activity entered the background.
    Paused,
    /// `SurfaceView` dimensions or system insets changed.
    Layout,
}

/// Forwards the Java callback identified by `kind` to the activity's winit loop.
pub fn dispatch(
    env: &Env<'_>,
    sender: &JObject<'_>,
    kind: i32,
    token: i64,
    sequence: i64,
    positions: [i32; 4],
    text: String,
    extra: String,
) {
    let event = match kind {
        1 => Event::Open,
        2 => Event::Save,
        3 => Event::Keyboard,
        4 => Event::Undo,
        5 => Event::Input {
            token,
            sequence,
            positions,
            text,
        },
        6 => Event::Opened {
            token,
            path: text.into(),
            uri: extra,
            writable: positions[0] != 0,
        },
        7 => Event::OpenEnded(token, positions[0]),
        8 => Event::Saved(token, positions[0]),
        9 => Event::Paused,
        11 => Event::Layout,
        12 => Event::ExportDraft,
        13 => Event::ExportOriginal,
        14 => Event::ExportEnded(token, positions[0]),
        15 => Event::InputRecentered(token, positions[0] == 0),
        _ => return,
    };
    if let Some((activity, proxy)) = PROXY
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .as_ref()
        && env
            .is_same_object(sender, activity.object.as_ref())
            .unwrap_or(false)
    {
        let _ = proxy.send_event(Envelope {
            to: Recipient::Active,
            event: UserEvent::Android(event),
        });
    }
}

/// Runs one Android activity using the shared winc editor.
pub fn run(app: AndroidApp) {
    let started = std::time::Instant::now();
    if let Err(error) = run_activity(app, started) {
        eprintln!("winc: Android activity: {error}");
    }
}

fn run_activity(
    app: AndroidApp,
    started: std::time::Instant,
) -> Result<(), Box<dyn std::error::Error>> {
    let root = app
        .internal_data_path()
        .ok_or("Android app has no private files directory")?
        .join("winc");
    let activity = Arc::new(Activity::new(&app)?);
    let event_loop = EventLoop::<Envelope>::with_user_event()
        .with_android_app(app)
        .build()?;
    let proxy = event_loop.create_proxy();
    let mut winc = Winc::new(Some(proxy.clone()), None);
    let (key, window_proxy) = winc.reserve();

    let config_path = root.join("config.json");
    let (mut config, problems) = Config::load_from(&config_path);
    for problem in problems {
        eprintln!("winc: {problem}");
    }
    config.git_enabled = Some(false);
    config.terminal_enabled = Some(false);
    config.remote_enabled = Some(false);
    config.format_on_save = Some(false);
    config.auto_save = Some(AutoSave::Off);
    config.vim_enabled = Some(false);
    crate::set_locale(config.language.unwrap_or_default().resolve());
    let mut editor = Editor::default();
    editor.tab_width = config.tab_width.unwrap_or(editor.tab_width);

    let state_path = root.join("state.json");
    let mut ui = App::new(editor, window_proxy, Bench::off(), config);
    ui.state = State::load_from(&state_path);
    ui.state.setup_pending = false;
    ui.store.config_path = Some(config_path);
    ui.store.state_path = Some(state_path);
    ui.backup.root = Some(root.join("backups"));
    ui.android.activity = Some(activity.clone());
    ui.android.started = Some(started);
    winc.add(key, ui);
    *PROXY.lock().unwrap_or_else(|error| error.into_inner()) = Some((activity, proxy));
    let result = event_loop.run_app(&mut winc);
    *PROXY.lock().unwrap_or_else(|error| error.into_inner()) = None;
    result?;
    Ok(())
}

pub(crate) struct Activity {
    vm: JavaVM,
    object: Global<JObject<'static>>,
}

impl Activity {
    #[allow(unsafe_code)]
    fn new(app: &AndroidApp) -> jni::errors::Result<Self> {
        // SAFETY: AndroidApp owns the JVM and Activity references for this activity.
        let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) };
        let object = vm.attach_current_thread(|env| {
            // SAFETY: The activity is a valid JNI global reference. Retain our own global ref.
            let activity = unsafe { JObject::from_raw(env, app.activity_as_ptr().cast()) };
            env.new_global_ref(&activity)
        })?;
        Ok(Self { vm, object })
    }

    fn with_java(
        &self,
        f: impl FnOnce(&mut Env<'_>, &JObject<'static>) -> jni::errors::Result<()>,
    ) -> bool {
        match self.vm.attach_current_thread_for_scope(|env| {
            env.with_local_frame(12, |env| f(env, self.object.as_ref()))
        }) {
            Ok(()) => true,
            Err(error) => {
                eprintln!("winc: Android bridge: {error}");
                false
            }
        }
    }

    pub(crate) fn pick(&self, token: i64) -> bool {
        self.with_java(|e, o| {
            e.call_method(
                o,
                jni_str!("pickDocument"),
                jni_sig!("(J)V"),
                &[JValue::Long(token)],
            )?;
            Ok(())
        })
    }

    pub(crate) fn save(&self, token: i64, uri: &str, baseline: &str, staged: &str) -> bool {
        self.with_java(|e, o| {
            let uri = e.new_string(uri)?;
            let baseline = e.new_string(baseline)?;
            let staged = e.new_string(staged)?;
            e.call_method(
                o,
                jni_str!("saveDocument"),
                jni_sig!("(JLjava/lang/String;Ljava/lang/String;Ljava/lang/String;)V"),
                &[
                    JValue::Long(token),
                    JValue::Object(&uri),
                    JValue::Object(&baseline),
                    JValue::Object(&staged),
                ],
            )?;
            Ok(())
        })
    }

    pub(crate) fn input(&self, token: i64, text: &str, anchor: i32, head: i32, show: bool) -> bool {
        self.with_java(|e, o| {
            let text = e.new_string(text)?;
            e.call_method(
                o,
                jni_str!("updateInput"),
                jni_sig!("(JLjava/lang/String;IIZ)V"),
                &[
                    JValue::Long(token),
                    JValue::Object(&text),
                    JValue::Int(anchor),
                    JValue::Int(head),
                    JValue::Bool(show),
                ],
            )?;
            Ok(())
        })
    }

    pub(crate) fn export(&self, token: i64, path: &str, excluded_uri: &str, name: &str) -> bool {
        self.with_java(|e, o| {
            let path = e.new_string(path)?;
            let excluded_uri = e.new_string(excluded_uri)?;
            let name = e.new_string(name)?;
            e.call_method(
                o,
                jni_str!("exportDocument"),
                jni_sig!("(JLjava/lang/String;Ljava/lang/String;Ljava/lang/String;)V"),
                &[
                    JValue::Long(token),
                    JValue::Object(&path),
                    JValue::Object(&excluded_uri),
                    JValue::Object(&name),
                ],
            )?;
            Ok(())
        })
    }

    pub(crate) fn recenter(
        &self,
        previous: (i64, i64),
        token: i64,
        text: &str,
        anchor: i32,
        head: i32,
    ) -> bool {
        self.with_java(|e, o| {
            let text = e.new_string(text)?;
            e.call_method(
                o,
                jni_str!("recenterInput"),
                jni_sig!("(JJJLjava/lang/String;II)V"),
                &[
                    JValue::Long(previous.0),
                    JValue::Long(previous.1),
                    JValue::Long(token),
                    JValue::Object(&text),
                    JValue::Int(anchor),
                    JValue::Int(head),
                ],
            )?;
            Ok(())
        })
    }

    pub(crate) fn hide_input(&self) {
        self.with_java(|e, o| {
            e.call_method(o, jni_str!("hideInput"), jni_sig!("()V"), &[])?;
            Ok(())
        });
    }

    pub(crate) fn first_frame(&self, millis: i64) {
        self.with_java(|e, o| {
            e.call_method(
                o,
                jni_str!("firstFrame"),
                jni_sig!("(J)V"),
                &[JValue::Long(millis)],
            )?;
            Ok(())
        });
    }
}
