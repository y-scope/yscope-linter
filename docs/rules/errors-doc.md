# `errors-doc`

Code: `DOC002`, in the `DOC` set. See the [rule index](README.md) for the codes of all rules.

The `# Errors` section of a fn's docstring must document every error the fn returns directly and
every function whose errors it forwards. It must not document errors the fn never returns, or
functions it never forwards.

The rule reads the fn's body with `syn`. It has no type information, so it decides what an error is
and where it comes from by the shape of the code and the names in it. This page describes those
decisions, so you can tell why a fn is flagged and what the fix is.

```rust
/// Loads the config.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`Error::NotFound`] if `path` is empty.
/// * Forwards [`read_file`]'s return values on failure.
fn load(path: &str) -> Result<Config, Error> {
    if path.is_empty() {
        return Err(Error::NotFound);
    }
    let text = read_file(path)?;
    parse(&text)
}
```

```console
$ yscope-linter example.rs
example.rs:14:5: DOC002 errors-doc: `load` forwards errors from `parse`, but `# Errors` does not list it; add "* Forwards [`parse`]'s return values on failure."
```

The tail call `parse(&text)` returns `parse`'s errors, so the section needs a bullet for it:

```rust
/// Loads the config.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`Error::NotFound`] if `path` is empty.
/// * Forwards [`read_file`]'s return values on failure.
/// * Forwards [`parse`]'s return values on failure.
fn load(path: &str) -> Result<Config, Error> {
    if path.is_empty() {
        return Err(Error::NotFound);
    }
    let text = read_file(path)?;
    parse(&text)
}
```

## Contents

* [Which fns are checked](#which-fns-are-checked)
* [The section format](#the-section-format)
* [Directly returned errors](#directly-returned-errors)
* [Forwarded errors](#forwarded-errors)
* [How calls and errors are matched to bullets](#how-calls-and-errors-are-matched-to-bullets)
* [The reverse checks](#the-reverse-checks)
* [Diagnostics](#diagnostics)
* [Known limits and when to suppress](#known-limits-and-when-to-suppress)

## Which fns are checked

A fn is checked when both of these hold:

* It returns a `Result`: its return type is a path whose last segment ends with `Result`, such as
  `Result<T, E>`, `io::Result<T>`, `anyhow::Result<T>` or `DbResult<T>`. An `async fn` that returns
  such a path counts too.
* Its docstring has an `# Errors` section. See [Section selection](#section-selection).

A fn that returns `impl Future<Output = R>`, where `R` is such a path, is checked too. Its body must
end with an `async` block, which is the part that is read. Without such a block, only the section's
format is checked.

These fns are checked:

* Free fns, and fns nested in other fns. A nested fn is checked on its own.
* Inherent methods and trait impl methods.
* Trait methods with a default body.

A trait method without a body documents a contract, not a body. Only the format of its bullets is
checked, and its section doesn't need the `Returns an error if:` intro:

```rust
trait Store {
    /// Loads a value.
    ///
    /// # Errors
    ///
    /// Implementations document their errors.
    fn load(&self) -> Result<Value, Error>;

    /// Saves a value.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`sqlx::Error`] on DB failure.
    fn save(&self) -> Result<(), Error>;
}
```

```console
$ yscope-linter example.rs
example.rs:15:5: DOC002 errors-doc: this Forwards bullet names no function; write "* Forwards [`path::to::func`]'s return values on failure."
```

These fns are not checked:

* A fn without an `# Errors` section.
* A fn whose return type isn't a `Result` path, such as `Option<T>`, an alias that doesn't end with
  `Result` (`Fallible<T>`), `Pin<Box<dyn Future<..>>>` or `Poll<Result<..>>`.
* Test code, if the rule skips it: with `--skip-tests` or a list that names the rule, such as
  `--skip-tests=errors-doc`, or with `skip-tests = true` or such a list in the [`Cargo.toml`
  settings](../../README.md#settings):
  * A fn with an attribute whose last path segment is `test`, such as `#[test]` or `#[tokio::test]`.
  * Any item marked `#[cfg(test)]` or `#[cfg(all(test, ..))]`, such as a `mod tests` block, and
    everything inside it.
  * A module file whose parent declares it with `#[cfg(test)] mod tests;`. The parent is looked up
    among `mod.rs`, `lib.rs` and `main.rs` in the same directory, and the sibling `<dir>.rs`.
    `#[path]` attributes are not followed.

Test code is checked by default, as any other code is:

```rust
/// Runs a check.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`load`]'s return values on failure.
#[test]
fn checks() -> Result<(), Error> {
    Ok(())
}
```

```console
$ yscope-linter example.rs
example.rs:7:1: DOC002 errors-doc: "Forwards [`load`]" names a function whose errors `checks` never forwards
```

With `--skip-tests` or `--skip-tests=errors-doc`, the rule skips `checks`.

The rule needs the file's syntax tree. A file that `syn` can't parse gets a `parse-error`
diagnostic, and the rule skips the file. When the rule skips test code, if a parent file that would
decide whether a file is a test module can't be read or parsed, the file is checked as non-test
code, and a `parse-error` at its first line says why. Each of these diagnostics is reported once per
file, and names every enabled rule it affects:

```text
src/store/tests.rs:1:1: LNT001 parse-error: can't tell whether this file is a `#[cfg(test)]` module: syn failed to parse `src/store/mod.rs` (cannot parse string into token stream); errors-doc, symbol-order, trait-params-doc, type-params-doc check it as non-test code
```

## The section format

The section follows the y-scope Rust style guide:

```text
/// # Errors
///
/// Returns an error if:
///
/// * [`Error::Variant1`] if <the condition>.
///   * <A sub-case, if there are several.>
/// * [`Error::Variant2`] if <the condition>.
/// * Forwards [`func`]'s return values on failure.
/// * Forwards [`other_func`]'s return values as [`Error::Variant3`] on failure.
```

### Section selection

* A header is an ATX header outside code fences. The section's header has the title `Errors`.
* The section starts at the `Errors` header of the smallest level, such as `# Errors`.
* If a header of a smaller level comes before it, the `Errors` header belongs to that other section,
  and the fn has no `# Errors` section.
* The section ends at the next header of the same or a smaller level.
* A second `Errors` header of the same level is reported. Merge the two sections.

So an `## Errors` subsection of `# Returns`, which documents the items of a returned stream, is not
the fn's section:

```rust
/// Streams the rows.
///
/// # Returns
///
/// A stream of rows.
///
/// ## Errors
///
/// Each item of the stream is an error if:
///
/// * [`RowError::Corrupted`] if a row is corrupted.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`Db::query`]'s return values on failure.
fn rows(db: &Db) -> Result<RowStream, Error> {
    db.query()
}
```

### The intro

The first line of the section must be exactly `Returns an error if:`, and at least one bullet must
follow it. Otherwise, the section gets an intro finding at its first line. These are flagged:

* Bullets without the intro line.
* A section of prose only, such as `Returns [`Error::Zstd`] if zstd compression fails.`.
* Another intro, such as `Returns [`Error::InvalidInput`] if:`.
* The intro without any bullet.

```rust
/// Compresses a buffer.
///
/// # Errors
///
/// Returns [`Error::Zstd`] if zstd compression fails.
fn compress(buffer: &[u8]) -> Result<Vec<u8>, Error> {
    if buffer.is_empty() {
        return Err(Error::Zstd);
    }
    Ok(Vec::new())
}
```

```console
$ yscope-linter example.rs
example.rs:5:1: DOC002 errors-doc: start the `# Errors` section with "Returns an error if:" followed by one bullet per error
```

There is one exception. A section without bullets is accepted in a fn whose body has no error source
at all. Such a section states that the fn never fails, for example to satisfy
`clippy::missing_errors_doc`:

```rust
/// Sends a heartbeat.
///
/// # Errors
///
/// This method does not currently return an error.
async fn heartbeat(&self) -> Result<(), Error> {
    self.registry.update().await;
    Ok(())
}
```

The wording isn't read. If the body can fail, the same section gets the intro finding.

### Variant bullets

A variant bullet documents errors that the fn returns directly.

* It starts with a link, optionally after `Returns a ` or `Returns an `.
* Its head is its text up to the first condition marker: ` if `, ` when `, ` for `, ` unless `, or a
  `:` that ends the bullet.
* Every link in the head is documented. So `` [`Error::Closed`] or [`Error::Expired`] if ... ``
  documents both, and is satisfied when the fn returns either of them.
* Its sub-bullets are conditions. Their links document no error.

A link is any of these:

* `` [`path`] ``, an intra-doc link.
* `` [`path`](target) `` and `` [`path`][label] ``, which name the code text `path`.
* `[text](path)`, which names the target.
* `[text][label]` and `[text]`, which name the target of the label's definition. The definition may
  be anywhere in the docstring, such as at its end.

```rust
/// Opens a session.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`Error::Closed`] or [`Error::Expired`] if the session is no longer usable.
/// * [`Error::Denied`] if:
///   * The user is unknown.
///   * The user is banned.
/// * [the timeout error](Error::Timeout) if the server doesn't answer.
/// * [the overload error][overloaded] if the server is busy.
///
/// [overloaded]: Error::Overloaded
fn open(session: &Session) -> Result<(), Error> {
    if session.is_closed() {
        return Err(Error::Closed);
    }
    if session.user().is_none() {
        return Err(Error::Denied);
    }
    if session.is_busy() {
        return Err(Error::Overloaded);
    }
    Err(Error::Timeout)
}
```

### Forwards bullets

A Forwards bullet documents a function whose errors the fn forwards. The canonical form is:

```text
/// * Forwards [`path::to::func`]'s return values on failure.
```

These forms are accepted too. The example below shows each of them:

* Several functions may share one possessive, as in the `validate` and `normalize` bullet.
* The mapped form names the variant that the errors are mapped into, as in the `Job::create` bullet.
* A list starts with `Forwards the return values of:`, followed by one sub-bullet per function. The
  first link of each sub-bullet is the function.
* A qualifier may follow `on failure`, such as `, except ...`, or ` if:` with sub-bullets. Text
  after ` except ` is ignored.
* A bare code span instead of a link may name a parameter of the fn, such as `decode`, which can't
  be linked.

```rust
/// Submits a job.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`validate`] and [`normalize`]'s return values on failure.
/// * Forwards [`Job::create`]'s return values as [`Error::BadRequest`] on failure.
/// * Forwards the return values of:
///   * [`Store::insert`]
///   * [`Queue::push`]
/// * Forwards [`notify`]'s return values on failure, except [`Error::Muted`].
/// * Forwards [`audit`]'s return values on failure if:
///   * Auditing is enabled.
fn submit(&self, request: Request) -> Result<(), Error> {
    validate(&request)?;
    normalize(&request)?;
    let job = Job::create(request).map_err(|_| Error::BadRequest)?;
    self.store.insert(&job)?;
    self.queue.push(&job)?;
    notify(&job)?;
    audit(&job)
}
```

A bullet that starts with `Forwards` or `Forward` but deviates from these forms gets a format
finding. See [Format findings](#format-findings).

### Other bullets and prose

A bullet that is neither a variant bullet nor a Forwards bullet, such as one that starts with a bare
code span, documents no variant. The links in it and in the section's prose still count as
documenting directly returned errors, but they are never reported as stale.

## Directly returned errors

An error is returned directly when the body builds it and returns it. Each one needs a variant
bullet, or a link in the section's prose.

### Return positions

An `Err(..)` counts only where its value leaves the fn:

* The fn's tail expression, a `return` value, or the operand of `?`.
* Inside any of these: a block's tail, an `if` or `else` branch, a `match` arm, and a `select!` arm.
* An argument that a value method may return, such as the `Err(..)` of `.map_or(Err(..), ..)`.
* The value and the `return` values of a closure that runs in place, such as the closure of
  `std::thread::scope(..)`, when the call is in a return position. See [Closures, async blocks and
  nested items](#closures-async-blocks-and-nested-items).

An `Err(..)` passed to a function, compared, asserted or matched against is not returned:

```rust
/// Cancels the pending requests.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`Store::clear`]'s return values on failure.
fn cancel(&self, tx: Sender<Result<(), Error>>) -> Result<(), Error> {
    tx.send(Err(Error::Cancelled));
    self.results.push(Err(Error::Timeout));
    match self.state() {
        Err(Error::Closed) => {}
        _ => {}
    }
    self.store.clear()
}
```

### What builds an error

These build a directly returned error:

* `Err(Error::Variant)`, `Err(Error::Variant(..))` and `Err(Error::Variant { .. })`.
* `.ok_or(Error::Variant)` and `.ok_or_else(|| Error::Variant)`. The receiver is an `Option`, so
  nothing is forwarded.
* A constructor call `Type::ctor(..)`, such as `Status::internal(..)` or
  `syn::Error::new_spanned(..)`. A bullet for `Type`, `Type::Ctor` or `Type::ctor` documents it.
* A variant wrapped in another, such as `Err(Outer::Inner(InnerError::Variant))`. Either variant
  documents it.
* The anyhow macros `bail!`, `ensure!`, and `anyhow!` or `format_err!` inside `Err(..)`. A bullet
  for `anyhow::Error` documents them.

```rust
/// Looks up a user.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`Error::EmptyName`] if `name` is empty.
fn find(&self, name: &str, id: u64) -> Result<User, Error> {
    if name.is_empty() {
        return Err(Error::EmptyName);
    }
    let user = self.cache.get(id).ok_or(Error::NotCached)?;
    let _ = self.users.get(name).ok_or_else(|| Error::UnknownUser { name: name.to_owned() })?;
    if user.is_banned() {
        Err(Error::Banned(id))
    } else {
        Ok(user)
    }
}
```

```console
$ yscope-linter example.rs
example.rs:12:35: DOC002 errors-doc: `find` returns [`Error::NotCached`] directly, but `# Errors` has no bullet for it; add "* [`Error::NotCached`] if ..."
example.rs:13:34: DOC002 errors-doc: `find` returns [`Error::UnknownUser`] directly, but `# Errors` has no bullet for it; add "* [`Error::UnknownUser`] if ..."
example.rs:15:9: DOC002 errors-doc: `find` returns [`Error::Banned`] directly, but `# Errors` has no bullet for it; add "* [`Error::Banned`] if ..."
```

```rust
/// Starts the server.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`anyhow::Error`] if `port` is 0.
/// * Forwards [`bind`]'s return values on failure.
fn start(port: u16) -> anyhow::Result<()> {
    if port == 0 {
        anyhow::bail!("port 0 is not allowed");
    }
    bind(port)?;
    Ok(())
}
```

### Conversions

`.into()`, `From::from`, `Type::from` and `?` convert an error into the fn's error type. The rule
can't see `From` impls in other files, so it asks for the error as written:

```rust
/// Loads the state.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`Error::Corrupted`] if the DB state is corrupted.
fn load_state(db: &Db) -> Result<State, Error> {
    if db.is_corrupted() {
        return Err(DbError::Corrupted.into());
    }
    Ok(State)
}
```

```console
$ yscope-linter example.rs
example.rs:7:1: DOC002 errors-doc: [`Error::Corrupted`] is documented, but `load_state` never returns it directly; if a callee returns it, write "* Forwards [`callee`]'s return values on failure." instead, or remove the bullet
example.rs:10:16: DOC002 errors-doc: `load_state` returns [`DbError::Corrupted`] directly, but `# Errors` has no bullet for it; add "* [`DbError::Corrupted`] if ..."
```

A head that names both types documents it:

```rust
/// Loads the state.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`Error`] converted from [`DbError::Corrupted`] if the DB state is corrupted.
fn load_state(db: &Db) -> Result<State, Error> {
    if db.is_corrupted() {
        return Err(DbError::Corrupted.into());
    }
    Ok(State)
}
```

When the same file declares the conversion, with a `#[from]` field or an `impl From<S> for T` whose
`from` builds variants of `T`, the converted variant documents the error too:

```rust
enum Error {
    Db(#[from] DbError),
}

/// Loads the state.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`Error::Db`] if the DB state is corrupted.
fn load_state(db: &Db) -> Result<State, Error> {
    if db.is_corrupted() {
        return Err(DbError::Corrupted.into());
    }
    Ok(State)
}
```

### Helpers

A call to a helper fn of the same file inside `Err(..)` is resolved one level deep. The helper's
tail and `return` expressions give the errors, and each needs a variant bullet. The helper returns
an error, not a `Result`, so it can't fail, and a Forwards bullet for it documents nothing:

```rust
fn transport_error(message: &str) -> Error {
    Error::Transport(message.to_owned())
}

/// Sends a message.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`Channel::write`]'s return values on failure.
fn send(channel: &Channel, message: &str) -> Result<(), Error> {
    if channel.is_closed() {
        return Err(transport_error("closed"));
    }
    channel.write(message)
}
```

```console
$ yscope-linter example.rs
example.rs:14:16: DOC002 errors-doc: `send` returns [`Error::Transport`] directly, but `# Errors` has no bullet for it; add "* [`Error::Transport`] if ..."
```

The fix here is a variant bullet, `` * [`Error::Transport`] if the channel is closed. ``.

A helper that can't be resolved, such as one in another file, makes the error unknown, which relaxes
the [reverse checks](#the-reverse-checks). The error still needs a bullet:

* When the helper fns of the file agree on a return type, a bullet that names the type or one of its
  variants documents the error.
* Otherwise, any variant bullet or prose link documents it.

```rust
fn transport_error(message: &str) -> Error {
    errors::build(message)
}

/// Sends a message.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`Channel::write`]'s return values on failure.
fn send(channel: &Channel, message: &str) -> Result<(), Error> {
    if channel.is_closed() {
        return Err(transport_error("closed"));
    }
    channel.write(message)
}
```

```console
$ yscope-linter example.rs
example.rs:14:16: DOC002 errors-doc: `send` returns the [`Error`] that `transport_error` builds, but `# Errors` has no bullet for it; add a bullet for the variant `transport_error` builds, or "* [`Error`] if ..."
```

## Forwarded errors

An error is forwarded when the body returns the error of a call unchanged, up to a `From`
conversion. Each forwarded function needs a Forwards bullet.

### What forwards

* `call()?` and `call().await?`, for fn calls and method calls.
* A call in a return position: the tail expression, or `return call()`.
* A binding of a call that is returned later: `let result = call(); result?`, or a tail `result`.
  `.map_err(..)`, `.context(..)`, `.with_context(..)` and the methods below that keep the receiver's
  error are peeled first, so `let result = call().context(..); result?` forwards `call`.
* A call of a closure bound with `let`, which forwards the call in the closure's value, after the
  same peeling. So `let create = |d| create_dir_all(d).context(".."); create(a)?` forwards
  `create_dir_all`.
* A `match` arm or an `if let` that passes the error on, as the `match` on `self.start(..)` below
  does.
* `.map_err(Into::into)`, `.map_err(From::from)` and `.map_err(Type::from)`.
* Methods that keep the receiver's error: `.map(..)`, `.inspect(..)` and `.inspect_err(..)`.
* Collecting the results of a mapping: in `.map(parse).collect::<Result<_, _>>()?`, the error comes
  from `parse`.
* `.and_then(f)`, which forwards both the receiver and `f`. `.or_else(f)` forwards only `f`.
* The futures of `try_join!(..)?`, and the arms of a `select!` in a return position.

```rust
/// Runs the job.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`Self::validate`]'s return values on failure.
fn run(&self, job: Job) -> Result<(), Error> {
    self.validate(&job)?;
    let plan = self.planner.plan(&job).await?;
    if plan.is_empty() {
        return self.skip(&job);
    }
    let result = self.executor.execute(plan).await;
    result?;
    match self.start(&job).await {
        Ok(_) | Err(Error::AlreadyRunning) => {}
        Err(error) => return Err(error.into()),
    }
    let ids = job.items.iter().map(parse_id).collect::<Result<Vec<_>, _>>()?;
    self.finish(ids)
}
```

```console
$ yscope-linter example.rs
example.rs:10:45: DOC002 errors-doc: `run` forwards errors from `plan`, but `# Errors` does not list it; add "* Forwards [`plan`]'s return values on failure."
example.rs:12:21: DOC002 errors-doc: `run` forwards errors from `skip`, but `# Errors` does not list it; add "* Forwards [`skip`]'s return values on failure."
example.rs:15:11: DOC002 errors-doc: `run` forwards errors from `execute`, but `# Errors` does not list it; add "* Forwards [`execute`]'s return values on failure."
example.rs:16:16: DOC002 errors-doc: `run` forwards errors from `start`, but `# Errors` does not list it; add "* Forwards [`start`]'s return values on failure."
example.rs:20:76: DOC002 errors-doc: `run` forwards errors from `parse_id`, but `# Errors` does not list it; add "* Forwards [`parse_id`]'s return values on failure."
example.rs:21:10: DOC002 errors-doc: `run` forwards errors from `finish`, but `# Errors` does not list it; add "* Forwards [`finish`]'s return values on failure."
```

### Mapped errors

`.map_err(f)` maps the callee's error into the errors that `f` builds. Either documentation is
accepted: a Forwards bullet for the callee, or a variant bullet for each error. The mapped form of a
Forwards bullet names both.

* `f` is a variant, such as `Error::Serialization`, or a closure that builds one.
* `f` is a helper of the same file, resolved as in [Helpers](#helpers).
* `f` can't be resolved, such as `to_status` from another file. Then a Forwards bullet for the
  callee, or any variant bullet at all, documents it.
* anyhow's `.context(..)` and `.with_context(..)` map into `anyhow::Error`.

```rust
/// Runs the job.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`store`]'s return values on failure.
fn run(&self, job: Job) -> Result<(), Error> {
    serialize(&job).map_err(Error::Serialization)?;
    compress(&job).map_err(|error| Error::Compression(error.to_string()))?;
    send(&job).map_err(to_status)?;
    store(&job).map_err(Into::into)?;
    Ok(())
}
```

```console
$ yscope-linter example.rs
example.rs:9:50: DOC002 errors-doc: `run` maps errors from `serialize` into [`Error::Serialization`], but `# Errors` documents neither; add "* Forwards [`serialize`]'s return values on failure." or "* [`Error::Serialization`] if ..."
example.rs:10:74: DOC002 errors-doc: `run` maps errors from `compress` into [`Error::Compression`], but `# Errors` documents neither; add "* Forwards [`compress`]'s return values on failure." or "* [`Error::Compression`] if ..."
example.rs:11:34: DOC002 errors-doc: `run` maps errors from `send` with `to_status`, but `# Errors` documents neither; add "* Forwards [`send`]'s return values on failure." or a variant bullet
```

This section documents each of them, in one of the accepted ways:

```rust
/// Runs the job.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`Error::Serialization`] if the job can't be serialized.
/// * Forwards [`compress`]'s return values as [`Error::Compression`] on failure.
/// * Forwards [`send`]'s return values on failure.
/// * Forwards [`store`]'s return values on failure.
fn run(&self, job: Job) -> Result<(), Error> {
    serialize(&job).map_err(Error::Serialization)?;
    compress(&job).map_err(|error| Error::Compression(error.to_string()))?;
    send(&job).map_err(to_status)?;
    store(&job).map_err(Into::into)?;
    Ok(())
}
```

### Closures, async blocks and nested items

A `?` inside a closure, an `async` block or a `try` block belongs to that closure or block, not to
the fn, unless the closure runs in place, as described below. Nested fn items are checked on their
own. So none of these is a forward of the fn:

```rust
/// Queues the upload.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`Queue::push`]'s return values on failure.
fn queue_upload(&self, job: Job) -> Result<(), Error> {
    let ids: Vec<u64> = job.items.iter().filter_map(|item| parse_id(item).ok()).collect();
    let task = async move {
        upload(&ids).await?;
        Ok::<(), Error>(())
    };
    self.queue.push(task)
}
```

A closure passed to a function is different when the function returns the closure's result. Then the
errors the closure builds leave the fn through that call. Either a Forwards bullet for the function
or a variant bullet for each of those errors documents it:

```rust
/// Runs the job.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`Store::insert`]'s return values on failure.
fn run(&self, job: Job) -> Result<(), Error> {
    self.store.insert(&job)?;
    retry(3, || {
        if self.is_closed() {
            return Err(Error::Closed);
        }
        Ok(())
    })
}
```

```console
$ yscope-linter example.rs
example.rs:10:5: DOC002 errors-doc: `run` forwards errors from `retry`, whose closures return [`Error::Closed`], but `# Errors` documents neither; add "* Forwards [`retry`]'s return values on failure." or a variant bullet for each closure error
```

A call that gets a closure or a fn path is also documented by a Forwards bullet for the function the
closure calls. So `call_with_retry(cfg, || async move { client.start_job(req).await })` is
documented by ``Forwards [`Client::start_job`]'s return values on failure.`` The closure's value is
peeled as in [What forwards](#what-forwards), so `call_with_retry(cfg, || start(r).context(".."))`
is documented by a Forwards bullet for `start`.

A closure that runs in place is part of the fn when the call's value leaves the fn, in a return
position. Its value is the call's value, so its `?` sites, `return`s, anyhow macros and value are
checked as the fn's own. Its value and `return` values leave the fn where the call does, so a `?`
after the call converts them with `From`, as it converts the call's value. Two kinds of closures run
in place, unless the closure is `async` or its body is an `async` block:

* The only argument of a fn whose last segment is `scope` or `block_in_place`, such as
  `std::thread::scope(|scope| ..)` and `tokio::task::block_in_place(|| ..)`.
* A closure called on the spot, `(|| ..)()`.

The call has no errors of its own, so a
``Forwards [`std::thread::scope`]'s return values on failure.`` bullet is reported as stale, and
the closure's callees are asked for instead.

The `?` sites of closures passed to `std::thread::spawn`, `tokio::spawn`, `scope.spawn(..)` and
other functions that run them elsewhere stay the closure's. So do those of a closure called on the
spot whose result the fn handles, as the `cleanup` below:

```rust
/// Runs the job.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`load`]'s return values on failure.
fn run(&self, job: Job) -> Result<(), Error> {
    let handle = std::thread::spawn(move || notify(&job.id));
    std::thread::scope(|scope| {
        scope.spawn(|| flush(&job));
        load(&job)?;
        store(&job)
    })?;
    let cleanup = (|| {
        remove(&job)?;
        Ok::<(), Error>(())
    })();
    if let Err(error) = cleanup {
        warn(error);
    }
    (|| {
        compress(&job)?;
        Ok(())
    })()
}
```

```console
$ yscope-linter example.rs
example.rs:14:7: DOC002 errors-doc: `run` forwards errors from `store`, but `# Errors` does not list it; add "* Forwards [`store`]'s return values on failure."
example.rs:23:23: DOC002 errors-doc: `run` forwards errors from `compress`, but `# Errors` does not list it; add "* Forwards [`compress`]'s return values on failure."
```

### Spawned tasks and other opaque sources

Some errors leave the fn from a source the rule can't name:

* `?` on a variable or a field, such as `self.pending?`.
* An awaited task handle, such as `handle.await?` after `let handle = tokio::spawn(..)`.
* The outer `?` of `x??`, except for `timeout(..)`.
* `?` on a macro call, such as `my_macro!(..)?`, other than `try_join!`.
* A `?` inside a macro whose arguments the rule can't parse, such as `tracing::info!(id = ?id, ..)`.

These need no bullet. They also relax the reverse checks:

* A Forwards bullet for a function that the body mentions is kept.
* A variant bullet that names an error type rather than a variant, such as `tokio::task::JoinError`,
  is kept.

```rust
/// Uploads the job in a task.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`tokio::task::JoinError`] if the task panics.
/// * Forwards [`upload`]'s return values on failure.
fn run(&self, job: Job) -> Result<(), Error> {
    let handle = tokio::spawn(async move { upload(job).await });
    handle.await??;
    Ok(())
}
```

### Pass-through helpers

A helper of the same file that returns the error it is given, unchanged, passes the callee's error
on. In this example, `classify` returns its `error` parameter when it isn't `Error::Sql`, so the fn
forwards `insert`:

```rust
fn classify(error: Error) -> Error {
    let Error::Sql(error) = error else {
        return error;
    };
    if is_too_large(&error) {
        return Error::PayloadRejected(error);
    }
    Error::Sql(error)
}

/// Inserts a row.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`Error::PayloadRejected`] if the row is too large.
fn insert_row(row: Row) -> Result<(), Error> {
    match insert(row) {
        Ok(()) => Ok(()),
        Err(error) => Err(classify(error)),
    }
}
```

```console
$ yscope-linter example.rs
example.rs:19:11: DOC002 errors-doc: `insert_row` forwards errors from `insert`, but `# Errors` does not list it; add "* Forwards [`insert`]'s return values on failure."
```

### Recursion

A call of the fn itself forwards nothing, so it needs no bullet. A Forwards bullet for it is stale.

In a trait impl method, a call of the method's own name through `Self::`, the impl's type or `self.`
calls the type's inherent fn of that name if the type has one, since Rust looks up inherent fns
before trait methods. Delegating a trait method to an inherent fn of the same name is common:

* If the file has an inherent impl of the type with a fn of that name, the call forwards that fn and
  needs a Forwards bullet.
* Otherwise the inherent fn may be in another file, so the bullet is optional: a bullet for it isn't
  stale, and a missing bullet isn't reported.

A call through the trait's name, such as `Submitter::submit(self)` or
`<Self as Submitter>::submit(self)`, is always recursion.

```rust
impl Client {
    fn submit(&self) -> Result<(), Error> {
        self.channel.send()
    }
}

impl Submitter for Client {
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`Error::Closed`] if the client is closed.
    fn submit(&self) -> Result<(), Error> {
        if self.is_closed() {
            return Err(Error::Closed);
        }
        Self::submit(self)
    }
}
```

```console
$ yscope-linter example.rs
example.rs:17:15: DOC002 errors-doc: `submit` forwards errors from `Self::submit`, but `# Errors` does not list it; add "* Forwards [`Self::submit`]'s return values on failure."
```

Without the inherent impl of `Client` in the file, the fn passes, with or without a bullet for
`Client::submit`.

The same holds for the call in a closure that runs in place, such as
`std::thread::scope(|_| Self::submit(self))`, and under an adaptor that passes the error on, such as
`.map_err(Into::into)`. Under `.context(..)`, `.with_context(..)` or a `.map_err(..)` that builds
errors, the mapped errors leave the fn whether or not the call is recursion. So the fn needs a bullet
for them or the Forwards bullet, as in [Mapped errors](#mapped-errors).

## How calls and errors are matched to bullets

Each side is reduced to a path of identifiers before matching:

* Generic arguments, a trailing `()` and a trailing `!` are dropped.
* A leading `::`, `crate`, `self` and `super` are dropped.
* `<T as Trait>::f` becomes `Trait::f`.
* A qualifier that is a type parameter of the fn or its impl, such as `T` in `T::new`, is dropped.

Two paths match when both of these hold:

* Their last segments are equal.
* If both have a qualifier, the second-to-last segment, the qualifiers are equal. `Self`, the impl's
  type and the implemented trait are equal to each other inside an impl.

So:

* A method call has no qualifier. So a bullet for `Store::load` or `load` matches
  `self.store.load(id)`.
* An unqualified bullet matches any qualifier. A bullet for `load` matches `Storage::load(id)`.
* A bullet for `Cache::load` doesn't match `Storage::load(id)`.
* In a method chain such as `collection.find(filter).with_options(options).await?`, the last method
  and the earlier methods of the chain all match. The call at the root of the chain doesn't.

```rust
/// Runs the job.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`Cache::load`]'s return values on failure.
fn run(&self, job: Job) -> Result<(), Error> {
    Storage::load(job.id)?;
    Ok(())
}
```

```console
$ yscope-linter example.rs
example.rs:7:1: DOC002 errors-doc: "Forwards [`Cache::load`]" names a function whose errors `run` never forwards
example.rs:9:26: DOC002 errors-doc: `run` forwards errors from `Storage::load`, but `# Errors` does not list it; add "* Forwards [`Storage::load`]'s return values on failure."
```

When no exact match exists, a second pass also accepts different qualifiers that the file links:

* A trait that the file declares or implements, or that a `use` item imports and the file never
  names again. This covers a trait imported only to call its methods.
* A rename, `use X as Y`, or an alias, `type Y = X`.

```rust
use spider_utils::config::YamlConfig;

/// Loads the server config.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`YamlConfig::from_yaml_file`]'s return values on failure.
fn load(path: &Path) -> Result<ServerConfig, Error> {
    ServerConfig::from_yaml_file(path)
}
```

Methods of `sqlx` query builders are qualified by the builder's type. The builder fns are `query`
and `query_with` (`Query`), `query_as` and `query_as_with` (`QueryAs`), and `query_scalar` and
`query_scalar_with` (`QueryScalar`). The type passes through `.bind(..)` and `.persistent(..)`:

```rust
/// Counts the jobs.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`sqlx::query::Query::fetch_one`]'s return values on failure.
fn count(&self, id: u64) -> Result<i64, Error> {
    let count = sqlx::query_scalar(SQL).bind(id).fetch_one(&self.pool).await?;
    Ok(count)
}
```

```console
$ yscope-linter example.rs
example.rs:7:1: DOC002 errors-doc: "Forwards [`sqlx::query::Query::fetch_one`]" names a function whose errors `count` never forwards
example.rs:9:77: DOC002 errors-doc: `count` forwards errors from `QueryScalar::fetch_one`, but `# Errors` does not list it; add "* Forwards [`QueryScalar::fetch_one`]'s return values on failure."
```

The fix is ``Forwards [`sqlx::query::QueryScalar::fetch_one`]'s return values on failure.``

## The reverse checks

The section must not document what the body doesn't do. These checks apply to bullets only, never to
links in prose.

### Stale variant bullets

A variant bullet is stale when none of its head links names an error that the body returns or
mentions, and none of its other links names a forwarded function:

```rust
/// Inserts the job.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`Error::Conflict`] if the job already exists.
/// * Forwards [`Store::insert`]'s return values on failure.
fn insert(&self, job: Job) -> Result<(), Error> {
    self.store.insert(&job)?;
    Ok(())
}
```

```console
$ yscope-linter example.rs
example.rs:7:1: DOC002 errors-doc: [`Error::Conflict`] is documented, but `insert` never returns it directly; if a callee returns it, write "* Forwards [`callee`]'s return values on failure." instead, or remove the bullet
```

A variant bullet is never stale in these cases:

* A head link names the fn's own error type, such as `Error` in a `Result<T, Error>` fn.
* The body maps an error with a mapper the rule can't resolve.
* The body has an opaque source, and the head names error types rather than variants, such as
  `tokio::task::JoinError`.
* A head link names a variant of a type `X` that the body converts into with `X::from`, and a `From`
  conversion into `X` may build it.
* A link after the head, or in a sub-bullet, names a function the body forwards. This documents a
  converted forward as the variant it becomes:

  ```text
  /// * [`CacheError::StaleState`] if:
  ///   * The job is terminal (see [`StateRequirement::ensure_met`]).
  ```

### Stale Forwards bullets

A Forwards bullet is stale when the body never forwards the function it names. A call whose result
the body only inspects doesn't count:

```rust
/// Runs the job.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`plan_events`]'s return values on failure.
fn run(&self, job: Job) -> Result<(), Error> {
    if let Some(events) = self.plan_events(&job) {
        self.queue.push(events);
    }
    Ok(())
}
```

```console
$ yscope-linter example.rs
example.rs:7:1: DOC002 errors-doc: "Forwards [`plan_events`]" names a function whose errors `run` never forwards
```

A Forwards bullet is kept in these cases:

* The function is forwarded inside a closure or an `async` block of the body, such as the `upload`
  of `self.spawn_body(async move { upload().await?; Ok(()) })`.
* The body mentions the function and has an opaque source or an unresolved mapper.
* The body has an opaque source, and some fn of the file calls the function. The opaque `Result` may
  come from that call, such as a task that another fn spawns.
* It names, in a bare code span, a callable parameter of the fn that the body mentions.
* It names the same-name fn that a trait impl method calls through `Self::`, its type or `self.`,
  and the file has no inherent fn of that name. See [Recursion](#recursion).

A Forwards bullet that names a type rather than a function gets a format finding instead.

### Delegation

When a fn both forwards a function without listing it and lists variants it never returns directly,
the section usually lists the callee's errors instead of forwarding the callee. These findings are
merged into one, at the section's header:

```rust
/// Submits a job.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`Error::JobNotFound`] if no job exists.
/// * [`Error::Transport`] if the transport fails.
fn submit(&self, job: Job) -> Result<(), Error> {
    self.orchestration.submit_job(job).await
}
```

```console
$ yscope-linter example.rs
example.rs:3:1: DOC002 errors-doc: `submit` lists 2 variants it never returns directly, and forwards `submit_job` without listing it; replace them with "* Forwards [`submit_job`]'s return values on failure."
```

## Diagnostics

Every diagnostic has the code `errors-doc`. Each one is reported once per error or callee, at its
first site:

* Missing errors point at the site: the `?`, the `Err`, the `ok_or` method, or the callee's name.
* Stale and format findings point at the bullet.
* Delegation and duplicate-section findings point at the header.

With `--diff` or `FILE:RANGES`, a diagnostic is kept when any line of its fn is selected, from its
first attribute or doc line to its closing brace. So deleting a bullet reports the forward it
documented.

### Missing documentation

| Message | Cause | Fix |
| ------- | ----- | --- |
| `` `f` returns [`E::V`] directly, but `# Errors` has no bullet for it; add "* [`E::V`] if ..." `` | The body returns `E::V`, and no variant bullet or prose link names it. | Add a variant bullet. |
| `` `f` forwards errors from `g`, but `# Errors` does not list it; add "* Forwards [`g`]'s return values on failure." `` | The body forwards `g`'s errors, and no Forwards bullet names `g`. | Add a Forwards bullet with `g`'s full path. |
| `` `f` maps errors from `g` into [`E::V`], but `# Errors` documents neither; add "* Forwards [`g`]'s return values on failure." or "* [`E::V`] if ..." `` | The body maps `g`'s error into `E::V`, such as with `.map_err(..)`. | Forward `g`, use the mapped form, or add a variant bullet. |
| `` `f` maps errors from `g` into [`E::A`] and [`E::B`], but `# Errors` documents only [`E::A`]; add "* Forwards [`g`]'s return values on failure." or a variant bullet for [`E::B`] `` | The mapper may build several variants, and only some are documented. | Forward `g`, or add the missing variants. |
| `` `f` maps errors from `g` with `m`, but `# Errors` documents neither; add "* Forwards [`g`]'s return values on failure." or a variant bullet `` | The mapper `m` can't be resolved. | Forward `g`, or add a variant bullet for what `m` builds. |
| `` `f` forwards errors from `g`, whose closures return [`E::V`], but `# Errors` documents neither; add "* Forwards [`g`]'s return values on failure." or a variant bullet for each closure error `` | A closure passed to `g` builds `E::V`, and `g` returns the closure's result. | Forward `g`, or add a variant bullet for each closure error. |
| `` `f` returns the [`E`] that `h` builds, but `# Errors` has no bullet for it; add a bullet for the variant `h` builds, or "* [`E`] if ..." `` | `Err(h(..))` returns an error that the helper `h` builds, and the scan can't resolve which. `E` is `h`'s return type. | Add a variant bullet for the error `h` builds. |
| `` `f` returns an error that `h` builds, but `# Errors` has no bullet for it; add a bullet for the variant `h` builds `` | The same, when `h`'s return type isn't known, such as for a helper in another file. | Add a variant bullet for the error `h` builds. |

```rust
fn classify(error: DbError) -> Error {
    if error.is_too_large() {
        return Error::PayloadRejected;
    }
    Error::Timeout
}

/// Inserts a row.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`Error::PayloadRejected`] if the row is too large.
fn insert_row(row: Row) -> Result<(), Error> {
    insert(row).map_err(classify)
}
```

```console
$ yscope-linter example.rs
example.rs:16:5: DOC002 errors-doc: `insert_row` maps errors from `insert` into [`Error::Timeout`] and [`Error::PayloadRejected`], but `# Errors` documents only [`Error::PayloadRejected`]; add "* Forwards [`insert`]'s return values on failure." or a variant bullet for [`Error::Timeout`]
```

### Stale documentation

| Message | Cause | Fix |
| ------- | ----- | --- |
| `` [`E::V`] is documented, but `f` never returns it directly; if a callee returns it, write "* Forwards [`callee`]'s return values on failure." instead, or remove the bullet `` | See [Stale variant bullets](#stale-variant-bullets). | Forward the callee that returns it, or remove the bullet. |
| `` "Forwards [`g`]" names a function whose errors `f` never forwards `` | See [Stale Forwards bullets](#stale-forwards-bullets). The body never calls `g`, or calls it without forwarding its error. | Name the function that is forwarded, or remove the bullet. |
| `` `f` lists 2 variants it never returns directly, and forwards `g` without listing it; replace them with "* Forwards [`g`]'s return values on failure." `` | See [Delegation](#delegation). | Replace the variant bullets with a Forwards bullet. |

### Format findings

| Message | Cause | Fix |
| ------- | ----- | --- |
| `` start the `# Errors` section with "Returns an error if:" followed by one bullet per error `` | See [The intro](#the-intro). | Start with the intro line and list each error as a bullet. |
| `` the docstring has a second `# Errors` section at the same level; merge them `` | Two `Errors` headers of the same level. | Merge the sections. |
| `` write "Forwards [`func`]'s return values on failure." `` | A Forwards bullet says `Forward`, uses a bare `'` possessive, or words what follows `'s` differently. | Use the canonical wording, or the mapped form. |
| `` this Forwards bullet names no function; write "* Forwards [`path::to::func`]'s return values on failure." `` | A `Forwards` bullet without a possessive link, such as `` Forwards [`sqlx::Error`] on DB failure. `` | Name the function whose errors are forwarded. |
| `` this Forwards bullet names no function; write "* Forwards `decode`'s return values on failure." `` | The same, when a bare code span in the bullet names a parameter of the fn. | Forward the parameter with a code span. |
| `` "Forwards [`tokio::task::JoinHandle`]" names a type, not a function; name the function whose errors are forwarded, or write "* [`tokio::task::JoinHandle`] if ..." for an error the fn returns `` | The named path's last segment is UpperCamelCase. | Name the function, or make it a variant bullet. |

```rust
/// Submits a job.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forward [`validate`]'s return values on failure.
/// * Forwards [`normalize`]' return values on failure.
/// * Forwards [`insert`]'s errors.
/// * Forwards [`sqlx::Error`] on DB failure.
/// * Forwards [`tokio::task::JoinHandle`]'s return values on failure.
fn submit(request: Request) -> Result<(), Error> {
    validate(&request)?;
    normalize(&request)?;
    insert(&request)?;
    Ok(())
}
```

```console
$ yscope-linter example.rs
example.rs:7:1: DOC002 errors-doc: write "Forwards [`func`]'s return values on failure."
example.rs:8:1: DOC002 errors-doc: write "Forwards [`func`]'s return values on failure."
example.rs:9:1: DOC002 errors-doc: write "Forwards [`func`]'s return values on failure."
example.rs:10:1: DOC002 errors-doc: this Forwards bullet names no function; write "* Forwards [`path::to::func`]'s return values on failure."
example.rs:11:1: DOC002 errors-doc: "Forwards [`tokio::task::JoinHandle`]" names a type, not a function; name the function whose errors are forwarded, or write "* [`tokio::task::JoinHandle`] if ..." for an error the fn returns
```

```rust
/// Decodes a frame.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards the error from `decode`.
fn decode_frame(decode: impl FnOnce() -> Result<(), Error>) -> Result<(), Error> {
    decode()
}
```

```console
$ yscope-linter example.rs
example.rs:7:1: DOC002 errors-doc: this Forwards bullet names no function; write "* Forwards `decode`'s return values on failure."
```

```rust
/// Loads the config.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`read_file`]'s return values on failure.
///
/// # Errors
///
/// * [`Error::Empty`] if the file is empty.
fn load(path: &str) -> Result<String, Error> {
    let text = read_file(path)?;
    if text.is_empty() {
        return Err(Error::Empty);
    }
    Ok(text)
}
```

```console
$ yscope-linter example.rs
example.rs:9:1: DOC002 errors-doc: the docstring has a second `# Errors` section at the same level; merge them
example.rs:15:16: DOC002 errors-doc: `load` returns [`Error::Empty`] directly, but `# Errors` has no bullet for it; add "* [`Error::Empty`] if ..."
```

### Other diagnostics

* `parse-error: syn failed to parse the file (...); errors-doc is skipped`: the rule can't check a
  file that `syn` can't parse. `comment-greedy-wrap` still checks it. When other rules that need
  the syntax tree are enabled too, the one diagnostic names them all, such as `errors-doc,
  symbol-order are skipped`.
* `` parse-error: can't tell whether this file is a `#[cfg(test)]` module: ... ``: the rule skips
  test code, and the parent file can't be read or parsed. See [Which fns are
  checked](#which-fns-are-checked).

## Known limits and when to suppress

The rule reads syntax, not types. These cases are known to be wrong or incomplete:

* Errors that flow through a variable, other than a binding of a call, are opaque. Nothing is
  required for them. Examples:
  * An `Err(..)` bound with `let` and returned later.
  * A `break Err(..)` value of a loop.
  * A variable reassigned after its `let`.
* Helpers and mappers in another file, or more than one level deep, can't be resolved.
* A `From` conversion in another file isn't seen. Name the inner error in the head, as in
  [Conversions](#conversions).
* Different functions with the same last segment, such as two `send` methods, satisfy each other's
  unqualified bullets.
* A Forwards bullet for a function that the body calls but handles, in a fn with an opaque source,
  isn't reported as stale.
* Query builder types are known only for the `sqlx` fns listed above.
* A callable argument named by an unqualified path that the file doesn't declare, such as
  `execute(cfg, imported_fn)`, isn't a spelling of the call. Forward `execute`.
* Return types are recognized by name only. An alias that doesn't end with `Result` isn't checked.
* A bound closure or call that ends in `.context(..)`, `.with_context(..)` or `.map_err(..)`,
  such as `let create = |d| create_dir_all(d).context(".."); create(a)?`, needs a Forwards bullet
  for the receiver's callee. A bullet for `anyhow::Error`, or for the variants the mapper builds,
  doesn't document it.
* A call of a closure bound with `let` forwards only the callee of the closure's value. The `?`
  sites, `return Err(..)`s and anyhow macros of its body aren't required, and a call whose result
  the fn handles adds nothing: only calls in a return position do. So after
  `let attempt = |x| { load(x)?; store(x) };`, `if attempt(1).is_err() { .. }` requires nothing,
  and `attempt(2)?` requires `store` only.
  [y-scope/yscope-linter#22](https://github.com/y-scope/yscope-linter/issues/22) tracks checking the
  body at those calls, as for in-place closures.
* A closure that runs in place keeps its `?` sites and `return`s to itself, and their errors aren't
  required, when its call isn't itself in a return position. Examples:
  * Its result is bound with `let` and returned later: `let result = (|| ..)(); result?`.
  * Its call is under `.context(..)`, `.with_context(..)` or `.map_err(..)`, as in
    `(|| ..)().context(..)?`. Only the call in the closure's value is required.
* Any fn whose last segment is `scope` or `block_in_place` and whose only argument is a closure is
  treated as running it in place.
* A `let` inside a closure replaces an outer binding of the same name for the rest of the fn. So in
  `let result = load(); std::thread::scope(|_| { result?; let result = store(); result })` at the
  fn's tail, `result?` is taken as a forward of `store`, and `load` isn't required.
* When the rule skips test code, out-of-line `#[cfg(test)]` modules declared with `#[path]`, or
  inside an inline module of the parent, are checked as non-test code.
* A trait impl method's same-name call through `Self::`, its type or `self.` is matched to an
  inherent fn of the file by the last segment of the impl's type only, and receiver kinds are
  ignored. Inherent impls in `#[cfg(test)]` code count too. So an inherent `submit` of an unrelated
  `Client` in a nested module, or a test-only inherent `submit`, makes the call of a non-test trait
  impl a forward, and the call of a `Box<Client>` impl is an optional forward although it is
  recursion.

When the rule is wrong for a fn, suppress it with a reason, rather than bending the docstring:

```rust
// yscope-linter: allow(errors-doc): the errors come from the callback the caller provides
/// Runs the callback.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards the callback's return values on failure.
fn run(&self) -> Result<(), Error> {
    (self.callback)()
}
```

An own-line `allow` above the fn's docstring, or anywhere from its first attribute or doc line
through its `fn` line, covers every finding of the fn. An own-line `allow` above a statement in the
body covers every finding of the statement, such as the `?` at the end of a multi-line chain. A
trailing `allow` covers the findings on its line. See [Suppression comments](../suppression.md).
