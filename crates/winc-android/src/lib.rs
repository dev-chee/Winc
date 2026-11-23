//! Entry points called by the Android activity and android-activity.

#![cfg(target_os = "android")]
#![allow(unsafe_code)]

use jni::EnvUnowned;
use jni::errors::ThrowRuntimeExAndDefault;
use jni::objects::{JObject, JString};
use jni::sys::{jint, jlong};
use winit::platform::android::activity::AndroidApp;

/// Starts winc on the native activity thread.
#[unsafe(no_mangle)]
pub fn android_main(app: AndroidApp) {
    winc_app::android::run(app);
}

/// Forwards an activity callback to the winit event loop.
#[unsafe(no_mangle)]
#[allow(non_snake_case)]
pub extern "system" fn Java_si_winc_android_WincActivity_sendNative<'local>(
    mut unowned: EnvUnowned<'local>,
    activity: JObject<'local>,
    kind: jint,
    token: jlong,
    sequence: jlong,
    a: jint,
    b: jint,
    c: jint,
    d: jint,
    text: JString<'local>,
    extra: JString<'local>,
) {
    unowned
        .with_env(|env| -> jni::errors::Result<()> {
            winc_app::android::dispatch(
                env,
                &activity,
                kind,
                token,
                sequence,
                [a, b, c, d],
                text.try_to_string(env)?,
                extra.try_to_string(env)?,
            );
            Ok(())
        })
        .resolve::<ThrowRuntimeExAndDefault>();
}
