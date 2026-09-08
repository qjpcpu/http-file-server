use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use crate::image_cache::{thumbnail_key, ImageCache};

const MAX_CONCURRENT_THUMBNAILS: usize = 8;
type Thumbnail = (Vec<u8>, &'static str);
type SharedResult = Result<Arc<Thumbnail>, Arc<io::Error>>;

#[derive(Default)]
struct Job {
    result: Mutex<Option<SharedResult>>,
    ready: Condvar,
}

#[derive(Default)]
struct State {
    active: usize,
    jobs: HashMap<String, Arc<Job>>,
}

#[derive(Default)]
struct ThumbnailGenerator {
    state: Mutex<State>,
    available: Condvar,
}

impl ThumbnailGenerator {
    fn generate(
        &self,
        key: String,
        render: impl FnOnce() -> io::Result<Thumbnail>,
    ) -> SharedResult {
        let mut state = self.state.lock().unwrap();
        if let Some(job) = state.jobs.get(&key).cloned() {
            drop(state);
            let mut result = job.result.lock().unwrap();
            while result.is_none() {
                result = job.ready.wait(result).unwrap();
            }
            return result.as_ref().unwrap().clone();
        }

        let job = Arc::new(Job::default());
        state.jobs.insert(key.clone(), job.clone());
        while state.active >= MAX_CONCURRENT_THUMBNAILS {
            state = self.available.wait(state).unwrap();
        }
        state.active += 1;
        drop(state);

        let result = render().map(Arc::new).map_err(Arc::new);
        *job.result.lock().unwrap() = Some(result.clone());
        job.ready.notify_all();

        let mut state = self.state.lock().unwrap();
        state.active -= 1;
        state.jobs.remove(&key);
        self.available.notify_one();
        result
    }
}

pub(crate) fn thumbnail(
    path: &Path,
    source: &[u8],
    max_edge: u32,
    cache: Option<&ImageCache>,
) -> SharedResult {
    static GENERATOR: OnceLock<ThumbnailGenerator> = OnceLock::new();
    GENERATOR.get_or_init(ThumbnailGenerator::default).generate(
        thumbnail_key(path, source, max_edge),
        || match cache {
            Some(cache) => cache.thumbnail(path, source, max_edge),
            None => crate::render_image_thumbnail(source, max_edge),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    fn wait_until(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(
                Instant::now() < deadline,
                "thumbnail workers did not reach the expected state"
            );
            thread::yield_now();
        }
    }

    #[test]
    fn limits_active_jobs_to_eight_and_runs_queued_jobs() {
        let generator = Arc::new(ThumbnailGenerator::default());
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Arc::new(Mutex::new(release_rx));
        let mut workers = Vec::new();
        for index in 0..16 {
            let generator = generator.clone();
            let active = active.clone();
            let peak = peak.clone();
            let started_tx = started_tx.clone();
            let release_rx = release_rx.clone();
            workers.push(thread::spawn(move || {
                generator
                    .generate(index.to_string(), || {
                        let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(count, Ordering::SeqCst);
                        started_tx.send(()).unwrap();
                        release_rx.lock().unwrap().recv().unwrap();
                        active.fetch_sub(1, Ordering::SeqCst);
                        Ok((vec![index], "image/jpeg"))
                    })
                    .unwrap()
            }));
        }
        for _ in 0..8 {
            started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        wait_until(|| generator.state.lock().unwrap().jobs.len() == 16);
        assert_eq!(active.load(Ordering::SeqCst), 8);
        assert!(matches!(
            started_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        for _ in 0..16 {
            release_tx.send(()).unwrap();
        }
        for (index, worker) in workers.into_iter().enumerate() {
            assert_eq!(worker.join().unwrap().0, vec![index as u8]);
        }
        assert_eq!(peak.load(Ordering::SeqCst), 8);
        let state = generator.state.lock().unwrap();
        assert_eq!(state.active, 0);
        assert!(state.jobs.is_empty());
    }

    #[test]
    fn concurrent_requests_share_success_and_failure_then_new_requests_run_again() {
        for fail in [false, true] {
            let generator = Arc::new(ThumbnailGenerator::default());
            let renders = Arc::new(AtomicUsize::new(0));
            let (started_tx, started_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            let leader = {
                let generator = generator.clone();
                let renders = renders.clone();
                thread::spawn(move || {
                    generator.generate("same-image".into(), || {
                        renders.fetch_add(1, Ordering::SeqCst);
                        started_tx.send(()).unwrap();
                        release_rx.recv().unwrap();
                        if fail {
                            Err(io::Error::new(io::ErrorKind::InvalidData, "invalid image"))
                        } else {
                            Ok((vec![42], "image/jpeg"))
                        }
                    })
                })
            };
            started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            let followers: Vec<_> = (0..8)
                .map(|_| {
                    let generator = generator.clone();
                    let renders = renders.clone();
                    thread::spawn(move || {
                        generator.generate("same-image".into(), || {
                            renders.fetch_add(1, Ordering::SeqCst);
                            Ok((vec![99], "image/jpeg"))
                        })
                    })
                })
                .collect();
            // The map and leader hold two references; each joined request
            // holds another while waiting for the shared result.
            wait_until(|| {
                Arc::strong_count(
                    generator
                        .state
                        .lock()
                        .unwrap()
                        .jobs
                        .get("same-image")
                        .unwrap(),
                ) == 10
            });
            release_tx.send(()).unwrap();
            let result = leader.join().unwrap();
            for follower in followers {
                match (&result, follower.join().unwrap()) {
                    (Ok(first), Ok(next)) => assert!(Arc::ptr_eq(first, &next)),
                    (Err(first), Err(next)) => assert!(Arc::ptr_eq(first, &next)),
                    _ => panic!("concurrent requests received different results"),
                }
            }
            assert_eq!(renders.load(Ordering::SeqCst), 1);
            let next = generator
                .generate("same-image".into(), || {
                    renders.fetch_add(1, Ordering::SeqCst);
                    Ok((vec![7], "image/jpeg"))
                })
                .unwrap();
            assert_eq!(next.0, vec![7]);
            assert_eq!(renders.load(Ordering::SeqCst), 2);
        }
    }

    #[test]
    fn concurrent_paths_sizes_and_source_versions_produce_separate_results() {
        let generator = Arc::new(ThumbnailGenerator::default());
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Arc::new(Mutex::new(release_rx));
        let inputs = [
            ("/a/photo.png", b"original".as_slice(), 128),
            ("/a/photo.png", b"replacement".as_slice(), 128),
            ("/a/photo.png", b"original".as_slice(), 512),
            ("/b/photo.png", b"original".as_slice(), 128),
        ];
        let workers: Vec<_> = inputs
            .into_iter()
            .enumerate()
            .map(|(index, (path, source, size))| {
                let generator = generator.clone();
                let started_tx = started_tx.clone();
                let release_rx = release_rx.clone();
                thread::spawn(move || {
                    generator
                        .generate(thumbnail_key(Path::new(path), source, size), || {
                            started_tx.send(()).unwrap();
                            release_rx.lock().unwrap().recv().unwrap();
                            Ok((vec![index as u8], "image/jpeg"))
                        })
                        .unwrap()
                })
            })
            .collect();
        for _ in 0..4 {
            started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        for _ in 0..4 {
            release_tx.send(()).unwrap();
        }
        for (index, worker) in workers.into_iter().enumerate() {
            assert_eq!(worker.join().unwrap().0, vec![index as u8]);
        }
    }
}
