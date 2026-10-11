//! Raises the priority of the threads that keep audio flowing, so a game
//! using every CPU core doesn't starve playback or the microphone.
//! Windows: registers the thread with MMCSS as "Pro Audio" (what audio apps
//! normally do), falling back to a higher thread priority. Elsewhere: nothing.

/// Keeps the raised priority until dropped (on the same thread).
#[must_use = "the priority is restored when this is dropped"]
pub struct AudioThreadPriority {
    #[cfg(windows)]
    mmcss: Option<windows::Win32::Foundation::HANDLE>,
}

pub fn raise_audio_thread_priority() -> AudioThreadPriority {
    #[cfg(windows)]
    {
        use windows::Win32::System::Threading::{
            AvSetMmThreadCharacteristicsW, GetCurrentThread, SetThreadPriority,
            THREAD_PRIORITY_HIGHEST,
        };
        let mut task_index = 0u32;
        // SAFETY: registers the calling thread; the task name is a static
        // wide string and `task_index` outlives the call.
        match unsafe {
            AvSetMmThreadCharacteristicsW(windows::core::w!("Pro Audio"), &mut task_index)
        } {
            Ok(handle) => {
                return AudioThreadPriority {
                    mmcss: Some(handle),
                }
            }
            Err(e) => {
                log::debug!("MMCSS unavailable ({e}); raising the thread priority instead");
                // SAFETY: GetCurrentThread is a pseudo-handle for this thread.
                if let Err(e) =
                    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) }
                {
                    log::debug!("couldn't raise the audio thread priority: {e}");
                }
            }
        }
        AudioThreadPriority { mmcss: None }
    }
    #[cfg(not(windows))]
    AudioThreadPriority {}
}

/// Raises the calling thread's priority for the rest of its life. For
/// threads we don't own, like cpal's device threads, where no guard can be
/// kept (it must be dropped on its own thread): call it from code running on
/// that thread, e.g. once in an audio callback. Later calls do nothing.
pub fn keep_audio_thread_priority() {
    thread_local! {
        // Dropped (priority restored) when the thread exits.
        static KEPT: std::cell::RefCell<Option<AudioThreadPriority>> =
            const { std::cell::RefCell::new(None) };
    }
    // Only fails while the thread is already exiting.
    let _ = KEPT.try_with(|kept| {
        let mut kept = kept.borrow_mut();
        if kept.is_none() {
            *kept = Some(raise_audio_thread_priority());
        }
    });
}

impl Drop for AudioThreadPriority {
    fn drop(&mut self) {
        #[cfg(windows)]
        if let Some(handle) = self.mmcss.take() {
            // SAFETY: `handle` came from AvSetMmThreadCharacteristicsW on this thread.
            let _ = unsafe {
                windows::Win32::System::Threading::AvRevertMmThreadCharacteristics(handle)
            };
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn raise_and_restore() {
        let guard = super::raise_audio_thread_priority();
        drop(guard);
    }

    #[cfg(windows)]
    fn current_priority() -> i32 {
        use windows::Win32::System::Threading::{GetCurrentThread, GetThreadPriority};
        // SAFETY: GetCurrentThread is a pseudo-handle for this thread.
        unsafe { GetThreadPriority(GetCurrentThread()) }
    }

    #[test]
    fn kept_priority_lasts_until_the_thread_ends() {
        let t = std::thread::spawn(|| {
            #[cfg(windows)]
            let before = current_priority();
            super::keep_audio_thread_priority();
            super::keep_audio_thread_priority();
            #[cfg(windows)]
            {
                // MMCSS reports 15 here; the fallback is 2 (HIGHEST).
                let after = current_priority();
                assert!(after > before, "{before} -> {after}");
            }
        });
        t.join().unwrap();
    }
}
