//! Raises the priority of the threads that keep audio flowing, so a game
//! using every CPU core doesn't starve playback or the microphone.
//! Windows: registers the thread with MMCSS as "Pro Audio" (what audio apps
//! normally do), falling back to a higher thread priority. Elsewhere: nothing.

/// Keeps the raised priority until dropped (on the same thread).
pub(crate) struct AudioThreadPriority {
    #[cfg(windows)]
    mmcss: Option<windows::Win32::Foundation::HANDLE>,
}

pub(crate) fn raise() -> AudioThreadPriority {
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
        let guard = super::raise();
        drop(guard);
    }
}
