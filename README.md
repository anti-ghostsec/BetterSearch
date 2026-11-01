# BetterSearch

A desktop booru search with real boolean searches and **links**: named
groups of tags on a post (`relation: rabbit · eat · carrot`) that you can
search for. Written in Rust; the window is [egui](https://github.com/emilk/egui).

```bash
cargo run --release -p bettersearch
```

It starts on **e926** (e621's safe-only site) with only *Safe* ticked. Pick
the sites and ratings from the two menus in the top bar. Add your name or e621 username in ⚙
Settings; e621 asks every app to say who runs it.

| Site | Account |
| --- | --- |
| e926, e621 | optional (API key) |
| Danbooru | optional: without one, searches take 2 tags (Gold 6, Platinum 12) |
| Safebooru | not needed |
| Gelbooru | required: user id and API key from its account options page |
| e6ai | optional (its own account; e621's API) |
| aibooru | optional (Danbooru's API) |
| xbooru, TBIB, Hypnohub | not needed |
| Realbooru (photos) | not needed; read from its pages, since its API was shut off years ago |
| yande.re, Konachan, Konachan.net, Sakugabooru | not needed (Moebooru) |

All of those are off until ticked in the site menu, and every one can be mixed
with the others.

Danbooru sits behind Cloudflare and throttles: a challenge or a 429 gets up to
three tries, waiting whatever `Retry-After` asks. A page that comes back with no
addresses at all is retried once after a pause, since that is usually a cut-back
answer rather than the real one.

**Custom boorus** (⚙ Settings → Custom boorus) take any site speaking one of
those four APIs: the address, a name, and the API family. https only, and the
address is the site root, not a page. They're searched like the built-ins and
can be removed again. Logins are optional and only for sites that need one.

`tools/probe-site.sh <host>` says in a few seconds whether a site is addable
and which family to pick, without changing anything.

## Taste

Off by default. When on, a **Guide** appears at the top of the left panel: chat
with it, and it answers knowing your tags, your brief and the searches you've
already run, so it doesn't send you round the same one again. Ask for picks and
it searches your own tags without a model. The model list is fetched from the
endpoint, so nothing has to be typed from memory.

**What leaves the machine, and that's all:** the top 40 weighted tags, and the
ratings you have ticked if "send the ratings I tick" is left on. No pictures,
no post ids, no history, no account. That payload goes to whichever endpoint is
set — Venice by default — with a plain User-Agent; the key stays in the vault.
Nothing is sent until you press **Write brief**, and briefs are capped by the
per-hour limit in the same section.

A search the guide proposes that finds nothing is retried once with the tags no
ticked site has (and any rating those sites can't carry) removed, and it says
which. The guide is also told which sites are in use and what ratings they take.

The searches themselves never involve the model: they are built from the tag
weights locally, so a bad brief can't send you somewhere you don't browse. The
model only writes prose.

## Local API

Off by default. ⚙ Settings → Local API lets a program on this computer search
and fetch pictures with a bearer token: `/api/v1/health`, `/openapi.json`,
`/search`, `/post/{site}/{id}`, `/file`, `/thumb`, `/taste`, and — only with
read-only switched off — `/favorite` and `/feedback`. It listens on loopback
unless "also from the local network" is ticked, is rate-limited, and logs paths
and statuses but not query strings.

The port and token go in `api.json` beside the vault (mode 0600) so a caller can
find them; nothing else is in it, and it's removed when the API is off. The
token can be replaced in Settings at any time.

`/file` exists because booru CDNs hotlink-protect and key-gate their URLs: the
app has the session and the keys, so the caller doesn't need either.

## Blocked sites

Rule34.xxx, furry.booru.org, safebooru.donmai.us and every `booru.org` site
answer an interactive challenge to plain clients.

Open the site once in a browser, then hand its cookie to the app (⚙ Settings →
Blocked sites): either **Copy cookies now**, which reads it out of a Firefox or
LibreWolf profile, or paste the `Cookie` header yourself. The cookie is kept in
the vault and sent to that host only, and searches work from then on until it
expires.

**Reading the browser is off by default** — with the box unticked the app opens
no browser files at all, and cookies come only from the button or a paste.
Tick it and a stale cookie is refreshed by itself before a search.

Wisp keeps its session in memory by default; turn on **Persistent profile** in
its menu, load the site, and its WebKit cookie store is read like a Firefox
profile. Otherwise paste. `cf_clearance` usually lasts hours, not days, and Cloudflare ties it to the
User-Agent of whoever earned it — so the UA is stored with the cookie and sent
with it: the browser's own for a Wisp or Firefox find, a Firefox one for a
pasted cookie. If the two don't match the site refuses it and it has to be
pasted again.

With automatic reading on, a stored cookie over ten minutes old is refreshed
again before a search, so it keeps working without you doing anything. Paheal answers HTML rather than its API.

On Danbooru and Gelbooru *Safe* means `general` (plus Safebooru's old
`safe`), and `sensitive` counts as *Questionable*. Danbooru's 2-tag limit
doesn't count rating, score, size, date, id or md5, so those always go to the
site; other tags are sent rarest first and the rest are checked here.
Gelbooru-style sites have no alias API and no tag categories in post answers,
so their tags all show as general. Some of them (TBIB) never send file
addresses; the folder and file name always are sent, so the addresses are
rebuilt from those. Realbooru has no API at all: its list pages
are read instead, and file addresses are rebuilt from the thumbnail's hash with
the site as referer.

## Searching

| Write | Finds |
| --- | --- |
| `fox wolf` | both tags (unless `fox_wolf` exists on the site, which wins) |
| `-sketch` | not this tag |
| `fox \| wolf` or `~fox ~wolf` | either |
| `(fox \| wolf) -(sketch \| comic)` | groups, nested as deep as you like |
| `fox*` | wildcard |
| `score:>100 rating:s order:score` | metatags |
| `[eat]` | posts where `eat` is in a link with other tags |
| `relation[rabbit eat]` | a `relation` link holding both |
| `@animal` | any tag in your group `animal` |
| `-@fat` | none of the tags in your group `fat` (sent to the site as `-a -b …`) |
| `favorited:yes` | your favorites (kept on this computer) |
| `rabbit eat relation` | rabbits eating what the relation index says rabbits eat |
| `@animal eat relation` | each animal in the group eating its own things |

Sites only take a flat list of tags with limits, so each search is **planned**
(open "How this search runs" to see the plan):

- Tags the site can take are sent to it, **rarest first** when there are more
  than its limit (e621: 40). The rest are checked here as posts load.
- One OR of plain tags goes as `~a ~b`. A bigger OR, like
  `(fox smile) | (wolf -sketch)`, runs as **one search per side, merged newest
  first**, with repeats dropped.
- **Old tag names**: a tag sent to the site counts as on every post the site
  sends for it, since the site follows its own aliases (Safebooru answers
  `golden_eyes` with `amber_eyes` posts). Tags checked here are swapped for the
  site's current names first, on sites that can say (e621, Danbooru).
- Every post that comes back is checked against the **whole** search, so the
  results are exact whatever the site could take.
- A link's tags go to the site like plain tags (they're all on its post), and
  the link itself is checked here. Posts saved on this computer are checked
  first. A search with nothing to send the site (only `-links`) runs on saved
  posts alone.
- The first page asks for about what fits on screen, not the site's whole page
  (320 posts take e621 three times as long as 60), and the next page loads
  while you look, so scrolling doesn't wait.

## Saved searches

**☆ Save** next to Search keeps the search with its sites and ratings, under a
name. They're listed at the top of the tags panel: click one to run it, ✕
then **Delete** to remove it. Each remembers the newest post it showed on
every site, so the next run marks posts that came in since with **NEW** and
counts them under the results.

Saved searches live in the encrypted vault like everything else. Deleting one
removes it and its query from your search history, and the database runs with
secure delete on, so deleted rows are overwritten rather than left in free
space; the next save writes a vault without them.

## Several sites at once

Tick more than one site in the site menu and a search runs on all of them, at
the same time, so it waits on the slowest site rather than each in turn.
Results take turns between the sites, and a file that turns up on two sites
(same MD5; Safebooru copies a lot of Danbooru) shows once.

Sites name things differently, so searches are written in **your** words and
**tag translations** say what to search on each site instead. Open them from
the site menu, or right-click a tag on a post → "Translate for other sites…".

| Your tag | Site | Search as |
| --- | --- | --- |
| `girl` | e926 | `female` |
| `girl` | Danbooru | `1girl \| 2girls` |
| `sketch` | Danbooru | *(empty: not on this site, left out there)* |

A translation can be any search without links. When several sites are picked
and one sends nothing at all, the app checks that site's tag counts and says
which tags it doesn't have, so you know where a translation is needed. Site aliases still
apply after translating (e621 turns `wolf_ears` into `ears` on its own).

## Browsing

- **Keys** (when you're not typing in a box): ← → move between posts, ↑ ↓ a
  row, Enter shows the selected post large, Esc goes back, F favorites, Z
  switches the large view between fit and full size.
- **Large view**: double-click a thumbnail (or Enter, or "View large"). ◀ ▶
  go through the results and load more at the end; "Original" shows the full
  file instead of the site's sample.
- **GIFs and videos play in the large view.** GIFs are decoded here, every
  frame. Videos (webm, mp4) are downloaded into memory, put in a RAM-only file
  that never touches the disk, and decoded by `ffmpeg` into frames the app
  draws, with sound (PulseAudio if running, else ALSA). Space pauses, M mutes,
  the bar seeks; videos loop. ffmpeg may only read that one file
  (`-protocol_whitelist file`), so a crafted video can't make it fetch
  anything. Needs `ffmpeg` installed; without it, "Open file" plays elsewhere.
- **Ticking posts**: the box at a thumbnail's top right (or Ctrl+click) ticks
  it, and a bar above the results acts on every ticked post at once:
  **↓ Download** saves each full file to the folder you last saved to
  (Downloads/BetterSearch at first), **♥ Favorite** adds them all (♡ takes them
  back out), and **Share** opens the FerroOS share box. One file goes to it
  straight from memory; several are saved like Download first, since the box
  takes several only as files. Share only shows on FerroOS, where
  `/usr/local/bin/ferro-share` exists. Right-click and the post panel still
  have their own Save file….
- **Tags in the post panel**: click to add one to the search, click again to
  take it out; − leaves it out. ✓ and ⊘ show what the search does with it.
- **Find similar** (post panel): posts sharing the most *telling* tags with
  this one. Rare tags count for more than everyday ones, and the post's
  species or character is required, so a dog playing guitar finds dogs
  (playing guitar, or dressed alike), not anyone in a plaid shirt.
  **Other versions** asks e621's picture search for edits, crops and reposts
  of the same picture (e621/e926 only; Danbooru's needs an account).
  "Back to search" returns to your search.

## Favorites

♥ in the post panel (or F, or right-click a thumbnail) keeps a post in your
favorites: in the encrypted vault, no account needed, with a copy of the post
so it's there offline. ♥ next to Search shows them from every site;
`favorited:yes fox` searches within them and `favorited:no` leaves them out.
The site is never told.

## The tags panel

The panel on the left (☰ Tags) shows:

- **In your search**: your tags as chips. Click one to flip it from wanted
  (✓) to excluded (⊘) to gone.
- **In these posts**: every tag on what's loaded, with a bar for how many
  posts have it. Click to search with it, − to exclude it. **Hover a tag** and
  the posts without it dim.
- **Your groups**, **links in these posts**, **your usual tags** (from your
  history) and **recent searches**.

## Relations: `rabbit eat relation`

- `rabbit eat` is plain tags: a rabbit eating anything, a steak included.
- `rabbit eat relation` (also `rabbit relation eat`, `link` or `@relation` in
  place of `relation`) uses the **relation index**: rabbits eating what
  rabbits eat, searched as `rabbit eating ~carrot ~vegetable …`.
- `@animal eat relation` pairs each animal in your group with *its own*
  things: dogs with what dogs eat, rabbits with carrots, never rabbits with
  bones.

**The index fills itself.** The first time you ask about a subject and verb,
the app learns from how tags appear together on the site (e621/e926 by
sampling random posts, Danbooru from its whole-site counts; ~15 s, once).
A good answer is tied to *both*: carrot comes with rabbit far more than with
posts in general, and comes with eating, directly or through the verb's big
companions (eating → food). Rabbit ears are tied only to rabbit, plates only
to eating; carrot is tied to both. The verb is looked up as the site tags it
(`eat` → `eating`, `ride` → `riding`).

Ties are judged cautiously (from the low end of what a sample could mean),
tags on 1% or more of the site are skipped as too broad, and when the verb
has bridges an answer must come with one (eaten things come with food; a
character's sweater seen at a few meals doesn't). On e926 that learns
rabbit + eat → vegetable, carrot; horse + eat → apple, muffin; a stray tag
or two still gets through. Dogs and cats learn little there: their eating
posts don't share a clear food in a 640-post sample. So: often right,
sometimes a stray, sometimes nothing. In the tags panel each
one has ✓ keep and ✕ reject; rejected ones are never used or learned again,
and relations you set up yourself count as kept. Groups & relations lists the
whole index with **Learn again**.

## Links, groups and relations

A **link** says how tags on a post go together: select a post, tick two or
more of its tags, pick a link name and press **Link** (`relation: rabbit ·
eat · carrot`). Add your own tags to a post in "Your tags"; they count in
searches like site tags.

**Links fill themselves in.** Once you've made a link, every post with the
same tags gets it too, marked *filled in*, with ✓ to keep it or ✕ to say "not
on this post" (it won't come back there). Turn this off in ⚙ Settings to get
the links offered with a + instead.

**Groups** name a set of tags and **relations** say how tags go together
everywhere, not just on one post. Open *Groups & relations* from the tags
panel, or right-click a tag → "Add to a group…":

| Set up | |
| --- | --- |
| group `animal` | dog, rabbit |
| group `object` | bone, carrot |
| relation | rabbit —eat→ carrot, dog —eat→ bone |

| Search | Finds |
| --- | --- |

| `@animal smile` | any animal, smiling (sent to the site as `~dog ~rabbit smile`) |
| `eat[@animal @object]`, `eat[rabbit]` | posts with a link (made by you or filled in) between them |

`link` and `relation` only ask with a subject and verb around them, so
`link zelda` is still the tag.

Under the hood the search becomes the links your relations allow
(`rabbit eat` → `eat[carrot rabbit] | eat[lettuce rabbit]`; open "How this
search runs" to see it). Pairs nothing relates are dropped, so the site is
only asked about real pairs, one search each, merged newest first. Relations
fill in under the site's own names too: e621 calls `dog` `domestic_dog`, and
dog —holding→ guitar still fills in on those posts. Posts saved here merge in
by age, so links using tags of your own still work.

A relation says the tags go together; it can't see the picture. `rabbit eat`
finds rabbits with carrots. To insist on the act, add the site's tag:
`rabbit eat eating`.

## Posts without images

e621 leaves the images out, for people who aren't logged in, on posts with
tags from its default blacklist (about one in eight explicit posts). Those
are skipped in the grid and counted under it; ⚙ Settings can show them
anyway. With your e621 username and API key in Settings, e621 sends them;
**Check logins** there asks each site whether it accepts your login. With an
API key but no login name, the name at the top of Settings is used.

**Gelbooru's images.** gelbooru.com's image servers serve the site itself only:
an app asking for an image is redirected to its post list. The app sends the
site as the `Referer` for its own image hosts, which is all it takes — the
pictures show like anywhere else. (Only to `gelbooru.com` hosts; nothing else
sees where a file came from.) Most Gelbooru posts are copies from Danbooru:
with both ticked, the same file shows once.

## Privacy: the vault

Everything the app keeps (settings and logins, search history, links,
groups, translations and the posts you've seen) lives in one encrypted file,
`~/.local/share/bettersearch/vault.bin`. On start the app asks for your PIN,
decrypts the file **into memory only**, and works on it there; it writes an
encrypted copy back a second after your own changes, every 30 seconds while
posts load, and on lock or exit. Nothing is written in the clear. Thumbnails
are only ever in memory.

- The key comes from your PIN with **Argon2id** (192 MiB, 3 passes, about a
  second per try); the data is sealed with **XChaCha20-Poly1305**, header
  included, so a wrong PIN or a changed byte is refused.
- **PINs are at least 6 characters.** Each guess costs someone who copies
  `vault.bin` about a second of one processor core, so every 4-digit PIN fell
  in about 3 hours; every 6-digit one takes about 12 days (less with many
  cores). A few words, or a longer mix of letters and digits, holds out for
  years. A vault set up when 4 was enough still opens with its PIN, then asks
  for a new one before anything else. Change it any time in ⚙ Settings.
- **There's no recovery.** Forget the PIN and the only way on is "Erase and
  start over" on the PIN screen.
- ⚙ Settings has **Lock now** and **Clear search history**. Deleting (a saved
  search, history, a link) overwrites the data inside the database, and each
  unlock compacts it, so nothing deleted is carried into the next vault.
- **One file.** `vault.bin` is the only thing the app writes in normal use.
  SQLite's temporary files, sorts and `VACUUM` copy are `MEMORY`; thumbnails,
  video playback (a `memfd`) and the local API's image cache never leave RAM.
  Setting the local API up may write one more file, `api.json`, holding only
  the port and its bearer token so another program can find it — no history,
  no view data, no account; it's removed when the API is switched off.
- Data from before the vault (`bettersearch.db`, `settings.json`) is moved in
  on first run and the plain files are deleted once the vault reads back.
  Deleting doesn't scrub the disk, so old copies may linger in free space.
- **Closing wipes what's readable.** The vault key, logins, API keys, the token
  and the cookie jar are zeroed on the way out, and SQLite's own memory is
  zeroed as it's freed. Post text and thumbnails are simply dropped; the
  kernel clears pages before another program gets them, but swap keeps copies
  until it is reused, so a hibernate still writes RAM to disk.
- **Locking clears the database from memory.** SQLite's memory (the database
  itself, its page cache, the copies made for saving) is zeroed as it's freed,
  and so is every decrypted or serialized copy of the vault, so locking or
  quitting doesn't leave it in freed memory for something else to read.
  SQLite's working space (sorting, compacting) stays in memory, never in
  temporary files. What the window is showing (posts, tags, text you typed) is
  ordinary memory and isn't wiped.
- While unlocked, the data is in the app's memory like any open document; the
  system could page it to swap.

What leaves the computer: searches, autocomplete words and image requests go
to the sites you tick, over https. Your name (from Settings) goes only to
e621/e926, which ask for it in the User-Agent; other sites get the app's name
alone. Logins go to their own site only (e621 and Danbooru in the request
header; Gelbooru's API wants them in the address, so network errors are shown
without addresses). Downloads are https-only, refuse redirects to plain http,
and stop at 400 MB. Links opened in the browser must be web links. Nothing
else is sent anywhere.

## Layout

```
crates/core   bettersearch-core: no GUI, reusable
  query.rs      the search language: parser, evaluator
  plan.rs       turns a search into site queries plus a local check
  search.rs     runs a plan on one site: translations, aliases, paging, filtering
  multi.rs      several sites together: takes turns, drops repeats by MD5
  sites/        one adapter per API family: e621/e926, Danbooru 2, Gelbooru 0.2 (Safebooru, gelbooru.com)
  taggraph/     tags, votes, links, aliases (Rust port of taggraph/)
  wipe.rs       SQLite's memory, zeroed as it's freed
  store.rs      the SQLite database: graph, post cache, history, groups, relations,
                favorites, saved searches, the relation index
  learn.rs      learns relations from how tags appear together
  similar.rs    posts sharing a post's most telling tags
crates/app    bettersearch: the egui window
  lock.rs       PIN screen and the saver
  vault.rs      the encrypted file (Argon2id + XChaCha20-Poly1305)
  media.rs      GIF and video playback (ffmpeg, from memory)
  tags_panel.rs the tags panel
  groups.rs     the Groups & relations window
taggraph/     the original TypeScript module from the website
```

`taggraph/` is the module from the website, kept for reference. The Rust port
keeps its schema, aliases, rename/merge and link votes, and drops what only a
public site needs (rate limits, blocking, adult flags; the rating filter does
that job here). Tag names use booru form (`long_hair`, not `long hair`).
Votes still record who made them, so a shared graph could sync later.

```bash
cargo test --workspace                     # 82 tests (63 core + 19 app); two use ffmpeg if installed, one live test is --ignored
cargo run -p bettersearch-core --example live -- "fox (smile | grin) -sketch"   # live, against e926
cargo run -p bettersearch-core --example live -- "fox_ears (smile | blush)" safebooru
cargo run -p bettersearch-core --example live -- "fox_ears smile -hat" danbooru
TRANSLATE="e926:fox_ears=fox" cargo run -p bettersearch-core --example live -- "fox_ears smile" e926,safebooru,danbooru
cargo run -p bettersearch-core --example live -- "rabbit eat relation"   # learns, prints the index
SIMILAR=6819203 cargo run -p bettersearch-core --example live -- x          # posts like #6819203
TAG_GROUPS="animal:fox,wolf;object:sword,guitar" RELATIONS="holding:fox,sword;holding:wolf,guitar" \
  cargo run -p bettersearch-core --example live -- "@animal holding @object"
```

## Adding a site

Implement `sites::Site` (search with a cursor, tag counts, aliases,
autocomplete, image fetch) and say what its search box can take in
`Capabilities` (including metatags that don't count toward its tag limit). The planner and the window need nothing else. Next up:

- Moebooru (Konachan, yande.re), more Danbooru and Gelbooru 0.2 sites
- blacklists with conditions
- thumbnails cached on disk (encrypted, like everything else)

## Licence

MIT, see [LICENSE](LICENSE).
