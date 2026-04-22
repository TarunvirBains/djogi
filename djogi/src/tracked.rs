use bytes::BytesMut;
use postgres_types::{FromSql, IsNull, ToSql, Type};

#[derive(Debug, Clone)]
pub struct Tracked<T> {
    value: T,
    dirty: bool,
}

impl<T> Tracked<T> {
    pub fn new(value: T) -> Self {
        Self {
            value,
            dirty: false,
        }
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Reset the dirty flag to `false`.
    ///
    /// Called by macro-emitted `save()` bodies after `RETURNING *` rehydration
    /// to ensure every `Tracked<T>` field is clean once `save()` returns.
    /// `from_pg_row` already constructs `Tracked::new(T)` with `dirty = false`,
    /// so this is defensive — but required by the Task 2 contract so that future
    /// in-place rehydration changes cannot silently break the invariant.
    ///
    /// `#[doc(hidden)]` — this is an implementation detail of the `#[model]`
    /// macro, not intended for direct caller use. Visibility is `pub` only
    /// because macro-emitted code runs in user crates outside `djogi`.
    #[doc(hidden)]
    pub fn mark_clean(&mut self) {
        self.dirty = false;
    }

    pub fn into_inner(self) -> T {
        self.value
    }
}

impl<T> std::ops::Deref for Tracked<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T> std::ops::DerefMut for Tracked<T> {
    fn deref_mut(&mut self) -> &mut T {
        self.dirty = true;
        &mut self.value
    }
}

impl<T> ToSql for Tracked<T>
where
    T: ToSql,
{
    fn to_sql(
        &self,
        ty: &Type,
        out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
        self.value.to_sql(ty, out)
    }

    fn accepts(ty: &Type) -> bool {
        T::accepts(ty)
    }

    postgres_types::to_sql_checked!();
}

impl<'a, T> FromSql<'a> for Tracked<T>
where
    T: FromSql<'a>,
{
    fn from_sql(
        ty: &Type,
        raw: &'a [u8],
    ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        T::from_sql(ty, raw).map(Tracked::new)
    }

    fn accepts(ty: &Type) -> bool {
        T::accepts(ty)
    }
}

impl<T> serde::Serialize for Tracked<T>
where
    T: serde::Serialize,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.value.serialize(serializer)
    }
}

impl<'de, T> serde::Deserialize<'de> for Tracked<T>
where
    T: serde::Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        T::deserialize(deserializer).map(Tracked::new)
    }
}

impl<T> Default for Tracked<T>
where
    T: Default,
{
    fn default() -> Self {
        Tracked::new(T::default())
    }
}

#[cfg(test)]
mod tests {
    use super::Tracked;

    #[test]
    fn new_is_clean() {
        let tracked = Tracked::new(String::from("alice"));
        assert!(!tracked.is_dirty());
    }

    #[test]
    fn deref_keeps_clean() {
        let tracked = Tracked::new(String::from("alice"));
        assert_eq!(&*tracked, "alice");
        assert!(!tracked.is_dirty());
    }

    #[test]
    fn deref_mut_marks_dirty() {
        let mut tracked = Tracked::new(1_i64);
        *tracked += 1;
        assert_eq!(*tracked, 2);
        assert!(tracked.is_dirty());
    }

    #[test]
    fn mark_clean_resets() {
        let mut tracked = Tracked::new(String::from("alice"));
        tracked.push_str(" smith");
        assert!(tracked.is_dirty());
        tracked.mark_clean();
        assert!(!tracked.is_dirty());
    }

    #[test]
    fn into_inner_returns_value() {
        let tracked = Tracked::new(String::from("alice"));
        assert_eq!(tracked.into_inner(), "alice");
    }
}
