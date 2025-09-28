use crate::error::Error;
use std::any::TypeId;
use std::collections::HashSet;
use std::marker::PhantomData;
use std::sync::{LazyLock, Mutex};

static USED_MARKERS: LazyLock<Mutex<HashSet<TypeId>>> = LazyLock::new(Default::default);

pub(crate) struct MarkerGuard<Marker>
where
    Marker: 'static,
{
    marker: PhantomData<Marker>,
}

impl<Marker> MarkerGuard<Marker>
where
    Marker: 'static,
{
    pub(crate) fn new() -> Result<Self, Error> {
        let mut used_markers = USED_MARKERS.lock().unwrap();
        if !used_markers.insert(TypeId::of::<Marker>()) {
            Err(Error::MarkerAlreadyUsed)
        } else {
            Ok(Self {
                marker: PhantomData,
            })
        }
    }
}

impl<Marker> Drop for MarkerGuard<Marker>
where
    Marker: 'static,
{
    fn drop(&mut self) {
        USED_MARKERS.lock().unwrap().remove(&TypeId::of::<Marker>());
    }
}
