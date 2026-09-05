# Batuta

A real-time NTFS file index for Windows: instant name search, live folder
sizes, and exact duplicate detection across millions of files.

Batuta does not walk directories. It reads and parses the NTFS Master File
Table directly, one sequential pass per volume, which is why a full index of
several million files takes seconds rather than minutes.

## Usage

```bash
cargo build --release
```

Then just run it. With no arguments it goes straight to Quick Setup, elevating
itself on the way — so double-clicking `batuta.exe` is enough:

```bash
batuta setup
```

A window Batuta opened for itself stays put until you press Enter, so a
double-clicked run does not flash its results past and vanish. That applies
only to setup and uninstall, which leave a report worth reading — the search UI
draws its own screen and closes cleanly when dismissed.

Quick Setup elevates itself, asks six questions, and leaves behind a working
install. Nothing on the machine changes until you confirm the summary:

1. install the daemon as a service for real-time indexing? (or a scheduled
   re-index every 1–60 minutes, or neither)
2. which drives to index
3. exclude Windows, Program Files and Program Files (x86)?
4. where to keep the index, whether to restrict it to your account, and
   whether to keep it in memory for fastest results
5. a keyboard shortcut to open the search bar
6. add `batuta` to `PATH`

`batuta uninstall` reverses all of it.

`batuta scan` asks the running daemon to rebuild, rather than scanning in its
own process. A separate scan would write a snapshot nobody reads: queries are
answered from the daemon's in-memory index, and its next checkpoint overwrites
the file. Only with no daemon running does `scan` do the work itself.

The individual commands still work on their own, from an elevated terminal:

```bash
batuta scan --verbose
```

```
batuta search <text> [--glob] [--ext rs] [--min-size 10M] [--under PATH] [--sort size]
batuta size <path> [--top 20]
batuta bloat [--top 30] [--under PATH]
batuta dupes [--min-size 1M] [--under PATH]
batuta status
batuta ui
batuta watch [--verbose] [--duration SECS]
batuta serve [--verbose]
batuta config [--init]
```

`scan` writes a snapshot to `%ProgramData%\Batuta\index.bin`, and every query
command reads that instead of re-reading the MFT. `--fresh` forces a rescan.

`serve` runs the resident daemon: it follows the change journal so folder sizes
stay current, and answers queries on a named pipe. While it is running, the CLI
uses it automatically and unelevated, so `batuta search` needs no Administrator
once a daemon is up. `watch` is the same tracking without the pipe, useful for
watching changes go by. `status` works for any user and needs no index.

## Interactive UI

`batuta ui` opens a terminal UI: type to search, with results updating as you
go.

| key | |
|---|---|
| any character | refine the search |
| `Ctrl+S` / `Ctrl+T` | cycle sort / filter by kind |
| `Shift+Tab` | switch between search, bloat and duplicates |
| `Tab` | complete to the highlighted directory, or a file's directory then the file |
| `Delete` | delete the highlighted file or folder, after confirming |
| `↑` `↓` `PgUp` `PgDn` `Home` `End` | move through results |
| `Ctrl+B` | show or hide the rail |
| `Ctrl+D` | jump straight to duplicates (and back) |
| `Ctrl+E` | open the explorer on the highlighted entry |
| `Ctrl+C` | open a terminal in the highlighted folder |
| `Ctrl+Q` | quit |
| `Enter` | reveal the selected entry in Explorer, and close |
| `Shift+Enter` | open it: a folder in Explorer, a file in its own application |
| `Ctrl+W` / `Ctrl+U` | delete a word / clear the query without closing |
| `Esc` | close |

Spaces are AND, matched against the whole path. `sso` searches names as it
always did; `sso updates` finds `D:\Downloads\SSO Updates\SSO 0.1.0.zip`
because one term is in the file's name and the other in a directory above it.
No single *name* contains "sso updates", so a name-only search could never find
that — yet it is the shape of most real searches, where you remember roughly
where a thing lives as well as roughly what it is called. Term order does not
matter, and every term has to match something on the path. Every occurrence
of every term is picked out in the result, so the reason a row is on screen
is visible rather than inferred.

Testing every node's whole path directly would mean millions of ancestor walks
per keystroke. Instead each term is scanned for once across the name arena —
the same SIMD pass a one-word search uses — and those per-name hits are pushed
down the tree in a single linear pass, so a node inherits every term its
ancestors matched. That leaves a small candidate set for the authoritative walk
to confirm. Typing a second word costs a few extra milliseconds, not a
different order of magnitude.

Typing a path browses it: `C:\` lists the root of the drive, `C:\Users\`
lists that directory, and `C:\Users\Dev` filters the directory to names
starting with `Dev`. If the path names no indexed directory, the last
segment falls back to a normal name search, so a typo still shows something.
`Tab` completes the drill-down: the highlighted directory's real path becomes
the query, with a trailing separator so you keep narrowing inside it. It works
from a plain name search too — you rarely know the path you want in advance,
which is the whole reason for searching — so `downloads`, `Tab` on the folder
you meant, and carry on typing inside it.

A file completes in two steps. The first `Tab` lands in the directory holding
it, which puts the siblings on screen and is usually what you were after; a
second `Tab` names the file itself. Going straight there would leave a query
matching only that one file, with nothing left to narrow. The file is
remembered across the first step rather than re-derived, because completing the
directory refetches and moves the selection off it — and any key other than
`Tab` drops the second step.

A rail down the left edge shows the mode, the kind filter and the sort order
all at once, with the active entry in each marked. These were previously
reachable only through a chord and reported only as text in the status line, so
the state you were in had to be remembered rather than seen. Nothing in it is
focusable: the query keeps the keyboard at all times, because typing is the
primary loop and a focus ring that swallowed keystrokes would be a regression.

The layout gives way in order of what matters least. A terminal under 100
columns drops the rail, because a file path needs the width more than a filter
list does, and the mode returns as pills in the frame instead. One too short to
show the rail's sections in full drops it as well, rather than leaving a `SORT`
heading with its options clipped off — a half-drawn rail claims state it is not
showing. Below sixteen rows the query box loses its frame too. `Ctrl+B` takes
the columns back by hand.

## Explorer

`Ctrl+E` opens a directory tree, a text editor and an editable path bar,
seeded from whatever is highlighted: a file opens in the editor with its
folder in the tree, a directory opens there. Finding something and then having
to leave to look at it was the gap — every other way out of the result list
hands the file to another program.

It is deliberately **not** in the `Shift+Tab` cycle. Cycling into an editor by
accident, or out of one holding unsaved changes, is a trap, and `Shift+Tab`
means outdent in every editor anyone has used. `Ctrl+E` goes in and `Ctrl+E`
comes back: the key that opened a view is the key that leaves it.

The explorer owns its keyboard outright rather than sharing the search view's.
That is not tidiness: out there `Delete` removes the highlighted **file from
disk**, `Ctrl+S` cycles the sort order, `Enter` opens Explorer and quits, and
`Esc` exits the program — all of which are wrong or dangerous with a text
buffer on screen. Guarding each one individually would leave the dangerous
ones a single missed guard away from firing.

| key | |
|---|---|
| arrows | move within a pane; cross to the next one at its edge |
| `Ctrl` + any arrow | move between panes regardless of where the caret is |
| `Enter` | expand a directory, open a file, or go to a typed path |
| `Shift+Enter` | open in the default application |
| `Ctrl+S` | save |
| `Ctrl+Z` / `Ctrl+Y` | undo / redo |
| `Ctrl+F` | find in the open file |
| `F3` / `Shift+F3` | next / previous match |
| `F5` | re-read the directory |
| `Ctrl+E` | back to search |

`Ctrl` with any arrow moves between panes outright, whatever the caret is
doing — down reaches the path bar, up comes back to whichever pane you left.

Arrow-crossing has two deliberate exceptions, both because the literal rule is
worse. The editor crosses left only at the very start of the buffer — at any
other line's column 0 it goes to the end of the line above, as every editor
does, because losing focus during the commonest motion in text editing is
infuriating. And the path bar never crosses sideways at all: it is a text
field, and Left and Right are how you edit one.

The tree reads the disk rather than the index, which is the only place in
Batuta that does. The index excludes `C:\Windows` and both `Program Files`
directories, and a file explorer that cannot show you `C:\Windows` is broken;
it can also be a snapshot, and a tree is exactly where staleness is noticed.
A directory that cannot be read says so on its row instead of appearing empty,
because an empty `C:\System Volume Information` is a lie.

**Opening a file and saving it untouched produces identical bytes.** Line
endings are kept per line, not normalised: files with mixed CRLF and LF are
real — a merge artefact, or one touched by both WSL and Notepad — and
rewriting the lines you did not touch turns a one-character fix into a
whole-file diff, and in a repository a whole-file conflict. The trailing
newline and a UTF-8 BOM are preserved exactly as found, never added and never
removed.

Whether a file is text is decided by more than a UTF-8 check, because `NUL` is
a legal codepoint and plenty of binaries decode cleanly: a NUL scan comes
first, then a control-character ratio. UTF-16 is refused **by name** — it is
common on Windows, from Notepad's "Unicode" or a PowerShell redirect, and
telling someone their file is invalid would be misleading. Anything refused
shows why, with `Shift+Enter` to open it in the application that does
understand it.

Saving writes a temp file beside the target, flushes it to the device, and
then calls `ReplaceFileW`. Not a rename: a rename hands the file the temp's
freshly inherited permissions, silently discarding any the user set
deliberately. The flush is what stops an atomic replace from committing an
empty file after a power cut. A file that changed on disk since it was opened,
or one marked read-only, is refused rather than overwritten.

Leaving with unsaved changes asks first, with three answers rather than two —
save, discard, or stay — and `Enter` deliberately does nothing, because it is
the key most likely to be hit from habit and one of those branches throws work
away.

`Ctrl+F` finds text in the open file, searching as you type and picking out
every hit on screen — the same treatment the result list gives matched terms,
for the same reason. `Enter` keeps the hit you landed on and closes the prompt;
`Esc` abandons the search and puts the cursor back where it started, so a
search that found nothing has not moved you. `F3` and `Shift+F3` step between
hits afterwards, wrapping at either end, and re-run the search each time so an
edit made in between cannot send you to a line the word has left.

`Ctrl+F` rather than `/`, which is the traditional spelling: in an editor a
slash is a character somebody is trying to type.

Syntax highlighting and formatters are out of scope by choice. Formatting means
shelling out to a per-language tool that has to be installed, found, and run
against a project it understands; it fails confusingly more often than it
works, and none of it is Batuta's job.

`Ctrl+C` opens a **terminal** in the highlighted folder, or the folder holding
the highlighted file. It is a real terminal — a pseudo-console with a VT
interpreter behind it — not a box that runs a command and prints the output. So
`git`, `cargo` and `npm` work, and so do the full-screen programs: colours,
cursor movement, scroll regions and the alternate screen are all handled.

`Esc` is forwarded rather than swallowed, because that is how anyone leaves
insert mode in `vim`.

### The shell it runs

The terminal does not open PowerShell. It runs **Batuta's own shell** — the
`batuta shell` subcommand, the same binary talking to itself through the
pseudo-console.

It is a small shell on purpose. It has:

- **Builtins** that need to change the shell itself, or that Windows has no
  program for: `cd` (with `cd -`), `pwd`, `ls`/`dir`, `cat`/`type`, `echo`,
  `mkdir`, `touch`, `cp`/`copy`, `mv`/`move`, `rm`/`del`, `which`, `set`,
  `clear`/`cls`, `help`, `exit`. Both spellings because both get typed.
  `rm`, `cp` and `mv` handle files only — a mistyped name that quietly
  duplicates or deletes a whole tree is not worth the convenience.
- **External programs**, found on `PATH` with `PATHEXT` applied and the current
  directory tried first, so `git`, `cargo` and `npm` work unchanged.
- **Pipes and redirection**: `a | b | c`, `> file`, `>> file`, `< file`,
  `2> file`, `2>&1`. Builtins take part in pipelines like anything else —
  `echo hi | cat` runs both in-process.
- **Quoting and expansion**: single and double quotes, `$VAR`, `${VAR}`,
  `%VAR%` and a leading `~`. A `$` inside single quotes survives, because that
  is the one place it must.
- **Line editing** it owns: `Left`/`Right`/`Home`/`End`, `Ctrl+U`, `Ctrl+W`,
  `Ctrl+L`, `Ctrl+C` to abandon a line, `Ctrl+D` to leave.
- **History** on `Up`/`Down`, which keeps the half-typed line you were on when
  you started walking back, ignores blanks and immediate repeats, and stops at
  1000 entries.
- **Tab completion** against the real filesystem, relative to the shell's
  directory rather than the process's — completing to the longest common
  prefix, and adding a separator when the single match is a directory.

What it deliberately does not have: control flow, functions, globbing,
subshells. This is a shell for running a few commands in a folder you just
found, not a language. PowerShell already exists and is better at being one —
and `powershell` is still one word away.

Setting `BATUTA_SHELL` overrides the choice entirely, for anyone who wants
their own.

`Ctrl+C` had to move for this. Inside a terminal it means *interrupt what is
running*, which is the one binding it would be perverse to take from someone,
so **quitting is `Ctrl+Q`** now. Inside the terminal only `Ctrl+Q` and `Ctrl+E`
are held back; everything else goes to the shell. `Shift+PageUp` and the mouse
wheel scroll the history.

`Esc` no longer closes anything in the explorer or the terminal. Reflexively
pressing it should not throw away an open editor, so the key that opened a view
is the key that leaves it. The one thing it still does is cancel the find
prompt, which closes a prompt rather than a view and so cannot lose any work.

Both views draw a **title bar** with minimise, maximise and close. That is not
decoration: the launcher window is deliberately borderless, which took the real
title bar away with the frame, and without these there is no way to get the
window out of the way short of ending the program. The buttons act on the
actual window, and the hit boxes are computed by the same function that decides
where to draw them, so the two cannot drift apart.

Color carries the reading order rather than decorating. Each mode has its
own accent that tints the border, the title, the mode pills in the top
corner, and the selected row, so you know where you are before reading
anything. Every colour comes from a token set rather than being named where it
is used, because a bare `Color::Cyan` is whatever the terminal decides it is and
lands invisible on some light schemes: the palette drops to the sixteen ANSI
colours when the terminal cannot do better, honours `NO_COLOR`, and picks
rounded borders only where they actually render rather than as replacement
boxes.

The interface paints its own background where the terminal can render 24-bit
colour: a base tone for the screen and a lighter one for each panel, so the
panes read as surfaces rather than as text floating on whatever was behind
them. Doing that removes the option of inheriting the terminal's foreground —
text that inherits is only legible against the background it was chosen for,
and ours is now a known dark tone — so background and foreground are always set
together, never one alone. In sixteen colours there is no tone subtle enough to
sit behind a frame without swallowing it, so those terminals keep inheriting
both, which is correct on any scheme. `BATUTA_NO_PANELS` opts out. Inside results, the part
of each path your keystrokes picked out
is bolded in the accent color, the rail's active entry in each section is a
filled bar rather than differently-coloured text, directories are tinted, sizes
warm toward
yellow and red as they become worth acting on, the bloat view draws a share
bar showing each directory's weight against the largest on screen, and the
duplicates view prints each group's reclaimable bytes in red — the number
the mode exists for.

Measured on a 3.4M-node index, each a cold process with no warm state to
help it:

| query | matches | time |
|---|---|---|
| `` (everything) | 3,401,288 | 76 ms |
| `a` | 1,880,259 | 95 ms |
| `no` | 81,071 | 21 ms |
| `node` | 18,454 | 11 ms |
| `node_m` | 3,122 | 8 ms |

Three things make it usable at this scale. Results are **windowed**: only the
rows on screen are ever requested or held, so a query matching half a million
files costs the same as one matching ten. The `Searcher` is **held across keystrokes**, so typing
forward narrows the previous result set instead of rescanning the name arena —
the property the whole search design was built around. And the **ordering is
cached**: scrolling changes only the offset, so a new window is sliced out of
the previous ordering rather than rescanning and re-sorting.

Fetches are debounced by a 30 ms gap in typing, and a burst of keystrokes is
drained before any query runs, so fast typing costs one query rather than one
per character.

Ordering is where this went wrong first. The default name sort lowercased both
sides of every comparison, allocating two `String`s each time, and — unlike the
size and date sorts — always ordered the entire result set rather than just the
page. On 3.4M matches that was a **9.4 second** stall per keystroke, which read
as a frozen UI. An allocation-free comparator, partial selection, and skipping
UTF-8 revalidation on each access brought it to 76 ms.

Reached by the hotkey it behaves as a launcher: a borderless, title-barless
panel centred on the work area rather than a full-screen command window. `Esc`
dismisses it in one press, and acting on a result closes it too. A failed open — a file the index
still lists but the disk no longer has — leaves the window up so the reason
stays readable.

It uses the daemon when one is listening, which means live folder sizes and no
Administrator needed. Otherwise it loads the snapshot itself and says so in the
status line.

`bloat` reports each directory's rolled-up TOTAL alongside OWN, the bytes held
directly in its own files. A directory that is large only because of one child
is rarely the one worth acting on; the two columns separate those cases, and a
share bar next to them shows each directory's weight against the largest one
on screen, so the shape of the list is legible without reading a number.

The duplicates scan runs off the UI thread. It reads file contents and takes
seconds, and doing that inline froze everything: no mode switch, no scrolling,
not even quitting until it finished. It is handed to a worker and collected
when ready, so the rest of the interface stays live while it runs — and because
it can now land while you are looking at something else, it fills its cache
without taking over that view's selection, window or counters.

The duplicates view (`Ctrl+D`, or Shift+Tab cycling) maps each byte-identical file against
the paths holding its copies: a divider row names the group — how many copies,
what each one is, and what deleting the extras would reclaim — followed by one
row per copy. The scan reads file contents and takes seconds, so it runs once
and is cached: scrolling and re-entering the view are instant, and `F5` is the
explicit way to rescan. Files under 1 MB are skipped, and only the 500
largest-waste groups are fetched, to bound both the scan and the wire. It goes
through the daemon like every other view, so it works with no index loaded
locally; the CLI's `batuta dupes` still runs standalone against the snapshot.

## Configuration

`%ProgramData%\Batuta\config.toml`, written by `batuta config --init`:

```toml
drives = ["C", "D"]
exclude = ['C:\Windows', 'C:\Program Files', 'C:\Program Files (x86)']
include_metadata = false
```

Exclusions are applied *after* the MFT is read, as a subtree prune. Reading the
MFT is one sequential pass regardless of what is kept, so skipping `C:\Windows`
would save nothing at scan time — and pruning afterwards means changing these
settings never requires a rescan.

## Design

```
crates/
  batuta-ntfs/   $MFT parsing, USN journal, volume I/O (windows-sys)
  batuta-core/   index, search, rollups, dupes, watch  (no OS dependencies)
  batuta-ipc/    wire protocol shared by both ends
  batuta-app/    CLI, TUI, daemon, named pipe          -> batuta.exe
```

`batuta-core` and the parsing half of `batuta-ntfs` touch no Windows APIs, so
they are tested against synthetic MFT records and synthetic trees without a
volume or elevation.

**Index layout.** Nodes are parallel arrays, not a `Vec<Node>`, so a pass that
only touches sizes never pulls names into cache. All base names live in one
contiguous UTF-8 arena in node order. MFT record number to node id is a
directly-indexed `Vec<u32>` rather than a hash map: NTFS record numbers are
dense, making it O(1) with no hashing at roughly 4 bytes per record instead of
~17.

**Search** has no inverted index. The name arena is small enough that a SIMD
substring scan crosses it in single-digit milliseconds on one core, and less
than a millisecond across all of them — an inverted index would cost tens of
megabytes resident to save time already below perception. Typing forward
narrows the previous result set instead of rescanning.

**MFT record numbers are recycled**, and NTFS bumps a 16-bit sequence number
each time. The index stores that sequence per node and checks it on every
change. Without it, a record freed by a deletion nobody saw stays mapped to the
old node, and the next file given that record is mistaken for it — the create
is discarded and the deleted entry never disappears. Because that failure feeds
on itself (each missed delete poisons a record for the next file to use it), an
index can degrade until new files stop showing up at all. A mismatched sequence
now retires the stale node and applies the change as new, so a missed event
heals instead of compounding.

**Folder sizes** are rolled up in one linear deepest-first pass. Keeping them
live then costs O(depth) per change — around 8 to 15 operations — rather than
re-walking anything.

**Duplicates** are found in three tiers: group by size (free, from the index),
then hash a 4 KB head and tail sample, then a full BLAKE3 hash only for what
survives both. Results are exact, while reading a small fraction of the data.
Hard-linked files are excluded, since deleting one frees nothing.

## Real-time tracking

A reader thread per volume blocks inside `FSCTL_READ_USN_JOURNAL`, so an idle
watcher parks in the kernel rather than polling. Each change costs O(depth) —
walking to the volume root updating totals — so sizes stay live without any
re-walk.

Keeping the name arena valid under mutation takes some care, because search
maps a match offset back to a node by binary searching `name_off` and that
requires the array to stay sorted. Creates append, so ordering holds. Deletes
tombstone rather than remove, since removing would renumber every later node.
Renames cannot rewrite a name in place, so the new name goes to a small
overflow table and the node is flagged; a snapshot folds those back in.

If the journal wraps past our position the daemon reports desynchronisation
and stops claiming to be live, rather than resuming from a gap.

**A volume with no USN journal cannot be followed** until one is created;
secondary and external drives often ship without one. `batuta status` reports
which volumes are live and which are not. Creating a journal modifies the
volume, so it is never done silently.

## Status

Working and verified against real volumes: MFT parsing, index construction,
size rollups, exclusions, search, duplicate detection, snapshot persistence,
USN change tracking, the daemon and its pipe protocol, the CLI and the TUI.

The daemon checkpoints its journal position as it goes, so an ungraceful stop
costs only a replay of the last minute rather than losing changes. If the
journal has moved past the stored position while the daemon was down, that is
reported and `status` stops claiming the index is live — a rescan is needed.

Known limitations:
- Checkpointing holds a read lock for the length of the write, which delays
  change application by about a second each minute. Nothing is lost — journal
  records queue in the kernel meanwhile.
- Timestamps are rendered in UTC, not local time.
- A query matching most of the volume still costs ~80 ms to order. Anything
  more specific than one or two characters is comfortably under 20 ms.
- Arena compaction only happens on snapshot writes.
- Recovering a spilled `$FILE_NAME` needs a rescan to take effect on an index
  built before the fix.
- The TUI has no treemap; a terminal cannot really draw one.

## Setup internals

A few decisions in the wizard are worth knowing about.

**Everything is asked before anything is applied.** Installing the service at
step 1, as the flow reads, would start a daemon before steps 2–4 had said which
drives to index or where to put the data — it would come up misconfigured and
need fixing seconds later. Collecting first also means backing out at the
summary leaves nothing half-installed.

**The binary is copied to a stable directory.** A service or scheduled task
pointed at `target\release` breaks on the next rebuild.

**`PATH` is edited through the registry, never with `setx`.** `setx` truncates
the value at 1024 characters and expands `%VAR%` references in place, which is
a well-known way to destroy someone's environment. The existing value is read,
appended to only when the entry is absent, and written back with its original
`REG_EXPAND_SZ` type intact.

**The hotkey needs its own small process.** A service runs as LocalSystem in
session 0 and cannot own a hotkey in your desktop session, and Windows `.lnk`
hotkeys only honour `Ctrl+Alt+<key>` reliably. So `batuta hotkey` registers the
combination and parks in `GetMessageW` — one thread blocked in the kernel, no
CPU. It starts from `HKCU\...\Run` rather than `HKLM`, because a hotkey belongs
to one session and a machine-wide entry would start a copy for every user who
logs in.

Signing in starts many programs at once, all claiming their shortcuts, and
whoever asks second is refused. Being refused used to kill the helper, so the
shortcut stayed dead until setup was run again by hand — and because Explorer
starts Run entries with a console attached, the dead helper left a window
sitting on the desktop for the rest of the session. It now detaches from that
console immediately, and retries a refused registration — briskly for the first
minute, then slowly — so it takes the combination over as soon as whatever held
it lets go. Anything it cannot do is written to `hotkey.log` beside the index,
since a background process has nowhere else to report and a shortcut that
silently does nothing is otherwise impossible to diagnose.

**The index is only rebuilt when it has to be.** Re-running setup to change a
shortcut should not cost a full rescan, so an existing snapshot is kept when it
covers exactly the drives you chose and is under an hour old. A changed drive
selection always forces a rebuild — reusing there would silently leave a volume
unindexed. The header and volume table are read on their own to decide this,
rather than loading the whole index file to look at a list of drive letters.

**The daemon is told who installed it.** It runs as LocalSystem, so asking its
own token who the user is would answer "SYSTEM" — and the named pipe would then
grant access to SYSTEM and Administrators only, locking out the unelevated CLI
the daemon exists to serve. Setup records the installing user's SID in the
config while it still runs as that user (UAC elevation keeps the user SID), and
the pipe grants that.

**Replacing the binary means stopping what is running it.** Windows locks a
running executable, so a second setup run cannot overwrite `batuta.exe` while
the service or hotkey helper holds it. Both are stopped first — the service
also has to restart anyway to pick up the new build, and only one process can
own a hotkey, so an old helper left alive would block the new one's
registration. If the file is still locked, it is renamed aside and replaced,
which Windows allows even for a running image. Identical content is skipped
entirely.

**Keeping the index in memory is a real choice, not a load switch.** The index
is always in memory while the service runs — that is what makes searches
instant, and what the resident footprint is. The setup question controls whether it
*stays* resident: answer no and the daemon releases its working set after 90
idle seconds, dropping to a few megabytes, at the cost of a slower first search
afterwards while the pages fault back in. The allocation is untouched either
way.

**The service check proves it works, not that it started.** A service can
report `RUNNING` having failed to open its pipe. Setup waits for an actual
`Status` round trip over the pipe before calling it good.

**One failing step does not abort the run.** A taken hotkey or an unwritable
`PATH` still leaves you with a working index and service, and the summary says
precisely which parts did not happen.

## Tests

```bash
cargo test --workspace
```

537 tests, none of which require elevation. The MFT parser is exercised against
hand-built records covering update-sequence fixups, resident and non-resident
`$DATA`, fragmented run lists, `$ATTRIBUTE_LIST` spill of both sizes *and*
names, hard links, DOS 8.3 aliases, alternate data streams, and deliberately
corrupted records.

One case is worth spelling out, because it was found on real data rather than
by construction. A directory with a large index can push its `$FILE_NAME`
attributes into an extension record. When the Win32 name goes and the 8.3 alias
stays, the entry ends up called `S-1-5-~1` and its real name is absent from the
index — so searching for it finds nothing. Names are now recovered from
extension records the same way sizes always were, and only replace the stored
name when they rank higher, so files that genuinely are called `FOO~1.DLL` are
left alone.

The IPC decoder gets the same treatment plus a fuzz sweep, since it parses
input an unprivileged client controls inside a privileged process: oversized
frames, lying string and row counts, invalid UTF-8, unknown tags and every
truncation of a valid message must all be refused rather than trusted.

The daemon's request loop is generic over its transport, so the whole
serve-a-client path — sequential requests, narrowing across them, malformed
frames, oversized frames, truncated streams — is tested against an in-memory
stream with no pipe and no elevation.

The setup wizard is tested as data. The step machine reads its answers through
a trait, so a scripted list drives the whole flow with no console: choosing the
service must skip the scheduled-task questions entirely, an interval of `0`,
`61` or `abc` must re-prompt rather than be accepted, and the summary shown
before confirming is rendered from the same struct the executor consumes, so
the two cannot drift apart. The generated `schtasks` command line, the `PATH`
edit and the ACL are unit-tested separately — the first two because quoting and
truncation are exactly where they break.

The TUI is tested too. Layout is a separate module from drawing, so the faults
that are invisible until they bite — a pane overlapping another, a rail clipped
to the point of hiding which sort is active, a results area collapsing to
nothing on a resize — are checked as arithmetic across a range of terminal
sizes, including ones too small to draw anything at all. The palette is decided
by a pure function over the environment, so `NO_COLOR`, a terminal that cannot
do more than sixteen colours, and one that renders rounded borders as
replacement boxes are all covered without needing that terminal. Scroll and
window arithmetic is likewise a separate module with no terminal involved, and rendering is checked against ratatui's `TestBackend`,
which catches layout faults — a status line pushed off screen, a row count that
disagrees with the space actually available — without needing a real terminal.
