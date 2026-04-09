<h1 align="center">Atommap</h1>

A convenient, safe, and performant API for atomic file I/O.

## The better interface for file I/O

The classic UNIX I/O interface: `read()`, `write()`, `seek()`.
It's the perfect interface for operating a magnetic tape.

But these days file I/O is mediated by the page cache[^direct],
which means these syscalls are not actually performing I/O at all -
they just modify an in-memory copy of the file.
The actual I/O happens when synchronising the page cache with the disk.
This might happen during the call to `read()`/`write()`, or it might be deferred until later, or it might have already happened.

The natural interface for interacting with the page cache is `mmap()`.
Instead memcpying between the page cache and a buffer, map the page cache into your address space and operate on it directly.
Having the whole file mapped into your address space is just so much nicer!
And it generally performs better to boot - often _much_ better.

With `read()`/`write()` you have to choose between convenience and performance:

* You could use a small buffer and operate on it one buffer-ful at a time.
  Efficient but painful!
  Reading a file via a small buffer is like peeking at it through a peephole.
  Parsing code grows complexity for dealing with partial buffers.
  And the optimal buffer size can't be known at compile time:
  it depends on the system you're running on.
* You could read the whole file into memory, operate on it, then write the whole thing back again.
  But if the file is large this wastes I/O bandwidth and memory,
  especially if you're only interested in a small part of the file.

With `mmap()` you get the best of both worlds:
it's _as if_ you've read the whole file into memory,
but data won't _actually_ be read in until you access it.
Once you stop accessing part of the file,
the data can be dropped to free up memory -
it'll be re-loaded from disk if you access it again.
You can freely modify the memory, and only the bits you touch get written back.

So what’s the catch?
There are a couple (see [below](#other-footguns)),
but let's start with the big one:
Other processes can modify the file while you have it mapped,
When that happens, the bytes in your address space spontaneously mutate
without you touching them.
It's effectively shared/volatile memory.
rustc assumes that memory doesn’t change unless your code changes it,
and violating this assumption leads to UB.
This is why `mmap()` generally requires an `unsafe` block to use from rust.

[^direct]: You can skip the page cache using direct I/O, but approximately no-one does that

## Private files can be mmapped safely

In Linux, you can create a file without linking it to the directory tree (ie. a file with no name and no filepath).
No path to the file means other processes can’t open it[^proc],
and that means the only process which can modify the file is the one that created it.
If we use the fd to create an mmap _and nothing else_
then all modifications to the file must be made via the mmap.[^io_safe]
This solves the "spontaneous mutation" problem.
Read more about [O_TMPFILE].

[^proc]: Unless they go snooping in /proc/$pid/fd.  But such adventures aren't covered by Rust's safety guarantees.
[^io_safe]: You might wonder about code in your own process `write()`ing to
  random fds.  Such behaviour is also [not covered][io safety] by Rust's safety
  guarantees: the tmpfile is an "exclusively owned fd", which "no other code is
  allowed to access in any way".

[O_TMPFILE]: https://man7.org/linux/man-pages/man2/open.2.html#:~:text=O%5FTMPFILE,-%28since
[io safety]: https://doc.rust-lang.org/nightly/std/io/index.html#io-safety

But typically we want to mmap an existing file, which is already linked to the directory tree.
Can we do that safely?
Yes: we create an unlinked _copy_ of the file and mmap that.
Then, when we're done, we replace the original file with the copy.

## Side-effect: everything is atomic now!

The data in the mmap reflects the state of the file at the instant we cloned it,
regardless of when it's faulted into memory.
Think "snapshot isolation".

Changes to the mmap are written to disk eagerly but aren't visible to other readers of the file
until you "commit" them (link the private clone back over the original file.)

## Clones are cheap (...on participating filesystems)

You might be thinking "copy the file..!?".
Well I have good news:
if your filesystem supports a feature called "reflinks" then
you can cheaply clone a file without actually copying any data.
On my (XFS-based) system it takes 0.1 ms to clone a file like this, regardless of its size.
Read more about [FICLONE].

That means the total cost of pulling this trick is 0.1 ms added to the initial setup,
which is small compared to the other setup costs (opening the file and mmapping it).
That's an easy sell!
..._if_ the filesystem supports reflinks.
Therein lies the rub:
if the file happens to be on a reflink-less filesystem then
we have to fallback to actually copying it, which is O(size).
Ouch, potentially.

Most new installs probably use XFS or btrfs,
but of course ext4 is common and will remain so for many years to come.
The conservative (Debian-based) distros are still defaulting to ext4 as of this writing!
But if you know that your code will be running exclusively on modern filesystems then you don't need to worry.

[FICLONE]: https://man7.org/linux/man-pages/man2/FICLONE.2const.html

## Putting it together

Don't write this:

```rust
let mut data = std::fs::read("foo.dat")?;
data.reverse();
std::fs::write("foo.dat", data)?;
```

(Runs in ...ms)

Or this:

```rust
let mut buf = vec![0; 4096];
let mut file = File::open("foo.dat");
loop {
    let n = file.read(&mut buf)?;
    if n == 0 { break; }
    buf[..n].reverse();
    file.seek(SeekFrom::Current(-n))?;
    file.write(&buf[..n])?;
}
```

(Runs in ...ms)

Instead, write this:

```rust
let mut data = Atommap::open("foo.dat")?;
data.reverse();
data.commit()?;
```

(Runs in ...ms)

TODO: Take measurements for the 3 above.

## Other footguns

mmap is great but it's not perfect.  Here are the drawbacks I'm aware of:

1. I/O errors are reported via a signal.
  A byte in the page cache actually has 257 possible states: 0-255, and "poison".
  "Poison" means there was a hardware problem and this region of the file is corrupt.
  `read()` checks for poison before copying and returns a nice error.
  With `mmap()` you get hit with SIGBUS.

  If your game plan for dealing with I/O errors was going to be to panic anyway, then this isn't _much_ of a downgrade.
  If you were planning to actually handle this kind of error then SIGBUS will make your life harder.
1. Unpredictable latency.
  This might be a turn-off for `async` users.

And here are some drawbacks of the "private clone" trick:

1. Writes increase disk usage until committed.
  This is unavoidable if you want atomic commits.
1. Reflinks increase file fragmentation.
  This... might be avoidable with more cleverness.

## Prior art

Immutable files can be safely mmapped without this trick.  That means files which:

* come from erofs/squashfs
* have fsverity enabled
* are a fully-sealed memfd

See [safe-mmap] for a crate that supports this use-case.

[safe-mmap]: https://crates.io/crates/safe-mmap
