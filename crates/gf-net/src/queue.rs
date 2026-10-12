//! The outgoing video queue policy (see the crate docs).

use std::collections::VecDeque;

use gf_proto::VideoFrame;

pub(crate) struct VideoQueue {
    frames: VecDeque<VideoFrame>,
    max: usize,
    waiting_for_key: bool,
    dropped: u64,
}

impl VideoQueue {
    pub fn new(max: usize) -> Self {
        VideoQueue {
            frames: VecDeque::new(),
            max,
            waiting_for_key: false,
            dropped: 0,
        }
    }

    /// Queues a frame. When `max` frames are already waiting, everything
    /// queued is discarded and frames are dropped until the next keyframe
    /// (a keyframe that overflows the queue is kept and starts it afresh).
    pub fn push(&mut self, frame: VideoFrame) {
        if self.waiting_for_key {
            if !frame.keyframe {
                self.dropped += 1;
                return;
            }
            self.waiting_for_key = false;
        }
        if self.frames.len() >= self.max {
            self.dropped += self.frames.len() as u64;
            self.frames.clear();
            if !frame.keyframe {
                self.dropped += 1;
                self.waiting_for_key = true;
                return;
            }
        }
        self.frames.push_back(frame);
    }

    /// Replaces whatever is queued with `group` (a keyframe and the frames
    /// after it), all of it even past `max`: pushed one by one, a group
    /// longer than `max` would be thrown away, keyframe and all.
    pub fn replace(&mut self, group: Vec<VideoFrame>) {
        self.frames = group.into();
        self.waiting_for_key = false;
    }

    pub fn pop(&mut self) -> Option<VideoFrame> {
        self.frames.pop_front()
    }

    /// Discards everything without counting drops (used when closing, and
    /// when a new share starts).
    pub fn clear(&mut self) {
        self.frames.clear();
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gf_proto::VideoCodec;

    fn frame(n: u64, keyframe: bool) -> VideoFrame {
        VideoFrame {
            codec: VideoCodec::H264,
            keyframe,
            sent_us: n,
            data: vec![n as u8],
        }
    }

    fn drain(q: &mut VideoQueue) -> Vec<u64> {
        std::iter::from_fn(|| q.pop()).map(|f| f.sent_us).collect()
    }

    #[test]
    fn keeps_order_below_limit() {
        let mut q = VideoQueue::new(3);
        for n in 0..3 {
            q.push(frame(n, n == 0));
        }
        assert_eq!(q.len(), 3);
        assert_eq!(drain(&mut q), vec![0, 1, 2]);
        assert_eq!(q.dropped(), 0);
        assert!(q.is_empty());
    }

    #[test]
    fn overflow_flushes_then_waits_for_keyframe() {
        let mut q = VideoQueue::new(3);
        for n in 0..3 {
            q.push(frame(n, n == 0));
        }
        // Fourth frame overflows: 3 queued + itself dropped.
        q.push(frame(3, false));
        assert_eq!(q.len(), 0);
        assert_eq!(q.dropped(), 4);
        // Delta frames keep being dropped until a keyframe arrives.
        q.push(frame(4, false));
        q.push(frame(5, false));
        assert_eq!(q.dropped(), 6);
        assert!(q.is_empty());
        q.push(frame(6, true));
        q.push(frame(7, false));
        assert_eq!(drain(&mut q), vec![6, 7]);
        assert_eq!(q.dropped(), 6);
    }

    #[test]
    fn overflowing_keyframe_is_kept() {
        let mut q = VideoQueue::new(2);
        q.push(frame(0, true));
        q.push(frame(1, false));
        q.push(frame(2, true));
        assert_eq!(q.dropped(), 2);
        q.push(frame(3, false));
        assert_eq!(drain(&mut q), vec![2, 3]);
    }

    #[test]
    fn replaced_group_is_kept_past_the_limit() {
        let mut q = VideoQueue::new(3);
        for n in 0..4 {
            q.push(frame(n, n == 0));
        }
        assert_eq!(q.dropped(), 4);
        q.replace((10..20).map(|n| frame(n, n == 10)).collect());
        assert_eq!(drain(&mut q), (10..20).collect::<Vec<_>>());
        // No longer waiting for a keyframe: the group was one.
        q.push(frame(20, false));
        assert_eq!(drain(&mut q), vec![20]);
        assert_eq!(q.dropped(), 4);
    }

    #[test]
    fn clear_does_not_count_drops() {
        let mut q = VideoQueue::new(5);
        q.push(frame(0, true));
        q.push(frame(1, false));
        q.clear();
        assert!(q.is_empty());
        assert_eq!(q.dropped(), 0);
        q.push(frame(2, false));
        assert_eq!(drain(&mut q), vec![2]);
    }

    #[test]
    fn writer_keeping_up_never_drops() {
        let mut q = VideoQueue::new(30);
        for n in 0..1000 {
            q.push(frame(n, n % 120 == 0));
            if n % 2 == 1 {
                q.pop();
                q.pop();
            }
        }
        assert_eq!(q.dropped(), 0);
    }
}
