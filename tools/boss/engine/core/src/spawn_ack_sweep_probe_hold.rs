//! Test-only latch so a caller can record a hook or pid while the
//! liveness probe is parked on the blocking pool.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};

pub struct ProbeHold {
    entered: Mutex<bool>,
    entered_cv: Condvar,
    released: Mutex<bool>,
    released_cv: Condvar,
}

impl ProbeHold {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            entered: Mutex::new(false),
            entered_cv: Condvar::new(),
            released: Mutex::new(false),
            released_cv: Condvar::new(),
        })
    }

    pub fn wait_for_entry(&self) {
        let mut g = self.entered.lock().unwrap();
        while !*g {
            g = self.entered_cv.wait(g).unwrap();
        }
    }

    fn wait(&self) {
        {
            let mut g = self.entered.lock().unwrap();
            *g = true;
            self.entered_cv.notify_all();
        }
        let mut g = self.released.lock().unwrap();
        while !*g {
            g = self.released_cv.wait(g).unwrap();
        }
    }

    pub fn release(&self) {
        let mut g = self.released.lock().unwrap();
        *g = true;
        self.released_cv.notify_all();
    }
}

static HOLDS: Mutex<Option<HashMap<String, Arc<ProbeHold>>>> = Mutex::new(None);

pub fn arm(execution_id: &str, hold: Arc<ProbeHold>) {
    HOLDS
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .insert(execution_id.to_owned(), hold);
}

pub fn disarm(execution_id: &str) {
    if let Some(map) = HOLDS.lock().unwrap().as_mut() {
        map.remove(execution_id);
    }
}

pub fn wait_if_armed(execution_id: &str) {
    let hold = HOLDS
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|map| map.get(execution_id).cloned());
    if let Some(hold) = hold {
        hold.wait();
    }
}
