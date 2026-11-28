//! The library APIs whose errors `errors-doc` knows how to attribute. Every table is matched by the
//! last segment of a fn path or by a method name only, so a same-named fn of any type is treated
//! the same way.

/// Methods whose receiver's error passes through unchanged. A member forwards its receiver's
/// callee, so `foo().map(..)?` forwards `foo`.
pub const PASS_THROUGH_METHODS: [&str; 3] = ["map", "inspect", "inspect_err"];

/// Methods that gather the results of their receiver's items. A member's errors are the errors of
/// the callable that produces the items, as [`ITEM_PRODUCING_METHODS`] and [`ITEM_KEEPING_METHODS`]
/// find it.
pub const SINK_METHODS: [&str; 6] = [
    "collect",
    "try_collect",
    "sum",
    "product",
    "transpose",
    "flatten",
];

/// Methods that may return a value taken out of their receiver, such as `.expect(..)` or
/// `.clone()`. A `Result` that a member returns comes from the receiver's value, which the scan
/// can't attribute.
pub const RECEIVER_VALUE_METHODS: [&str; 8] = [
    "expect",
    "unwrap",
    "unwrap_or",
    "unwrap_or_else",
    "unwrap_or_default",
    "clone",
    "cloned",
    "copied",
];

/// Methods that may return one of their arguments or the value of a closure argument, such as
/// `.map_or(Err(..), |_| Ok(()))`. A member's errors are the errors of those arguments.
pub const ARGUMENT_VALUE_METHODS: [&str; 4] =
    ["map_or", "map_or_else", "unwrap_or", "unwrap_or_else"];

/// Fns that gather the results of the futures their argument yields. A member's errors are the
/// errors of the callable that produces the futures.
pub const SINK_FNS: [&str; 2] = ["try_join_all", "join_all"];

/// Iterator and stream adaptors whose callable produces the items, such as `.map(f)`. A member's
/// callable is classified as the source of the items' errors.
pub const ITEM_PRODUCING_METHODS: [&str; 4] = ["map", "flat_map", "then", "and_then"];

/// Iterator and stream adaptors that keep their receiver's items, such as `.filter(..)`. The scan
/// looks through a member to the adaptor that produces the items.
pub const ITEM_KEEPING_METHODS: [&str; 13] = [
    "filter",
    "skip",
    "take",
    "rev",
    "fuse",
    "peekable",
    "inspect",
    "skip_while",
    "take_while",
    "step_by",
    "buffered",
    "buffer_unordered",
    "boxed",
];

/// The query builder fns of `sqlx`, with the type of the builder each one returns. A method called
/// on a member's builder is qualified by that type, such as `QueryAs::fetch_one`.
pub const BUILDER_FNS: [(&str, &str); 6] = [
    ("query", "Query"),
    ("query_as", "QueryAs"),
    ("query_scalar", "QueryScalar"),
    ("query_with", "Query"),
    ("query_as_with", "QueryAs"),
    ("query_scalar_with", "QueryScalar"),
];

/// The query builder methods that return the builder they are called on. A member keeps the type of
/// its receiver's builder.
pub const BUILDER_KEEPING_METHODS: [&str; 2] = ["bind", "persistent"];

/// Fns and methods whose awaited handle fails with a join error. `?` converts a member's join error
/// into an unknown variant of the fn's error type.
pub const SPAWN_FNS: [&str; 2] = ["spawn", "spawn_blocking"];

/// Fns that run their only argument, a closure, before they return, and return its value, such as
/// `std::thread::scope(|scope| ..)`. A member's closure is part of the fn when the call's value
/// leaves the fn.
pub const IN_PLACE_FNS: [&str; 2] = ["scope", "block_in_place"];

/// The traits whose implementors a fn may call or await to get a result. A parameter bounded by a
/// member is a callable parameter.
pub const CALLABLE_TRAITS: [&str; 8] = [
    "Fn",
    "FnMut",
    "FnOnce",
    "AsyncFn",
    "AsyncFnMut",
    "AsyncFnOnce",
    "Future",
    "IntoFuture",
];
