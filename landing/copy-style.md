# copy style

The rules for all user-facing text about tidepool: the landing page, the doc
site, the readme, and the documentation that the server shows (the doc
comments on the config types, the endpoint table, the MCP tool descriptions).

Two parts. Part 1 says what to write about. Part 2 says how to write it. The
rules are the same as kayak's (`landing/copy-style.md` in the kayak
repository). Only the positioning is different.

## 1. Positioning

### What tidepool is

tidepool is a live analytics server. It is one Rust binary with the web UI
in it. You push rows to it over HTTP. You declare the tables, the semantic
model and the dashboards in YAML files, and you keep the files in version
control. The dashboards update while data arrives. A click on a value filters
every number on the page. Users of Qlik know this model as "associative".

### The three things to show

1. **Live.** A row is on every open dashboard in less than a second after
   tidepool acknowledges it. tidepool updates each answer with only the new
   rows. Use the numbers in `product-and-copy.md` section 5, with the machine
   and the dataset.

2. **Associative.** A selection filters the whole dashboard. Each filter shows
   which values are selected, possible and excluded. Selections follow the
   relationships of the model, in one direction or in both. Trees (a plant, a
   category tree) select a whole branch. Back and forward go through the
   selections.

3. **The project is code.** Tables, the model (Apache Ossie) and dashboards are
   files. tidepool plans each change and gives it a class (safe, rebuild,
   destructive). It applies the change while it serves, and writes it back to
   the files and to git. The same pipeline takes changes from the files, the
   config page, the API and agents.

Also show that tidepool is **correct**: each query is compared with DuckDB
over random schemas and data, and the store survives `kill -9` without the
loss of acknowledged data. Check each claim against the code before you write
it.

### The web UI

The web UI is the product. The dashboards are what a user looks at. Show
them. The screenshots in `screenshots/` are for the landing page and for the
guide.

- In the guide, give the config first, and then what the user sees.
- For a task (a change of the model, a write of rows), give the file, the
  command or the HTTP request. The config page is an optional extra, after the
  file.

### The typical workflow (use this in examples)

1. Run `tidepool init shop`, or write the files of a project.
2. Commit them.
3. Run `tidepool serve --config shop`, or the container image with the
   project mounted.
4. Push rows over HTTP, or put kayak in front of a broker.
5. Change a file, read the plan (`tidepool plan`), and let the server apply it.

## 2. Language: ASD-STE100 Simplified Technical English

Apply STE wherever possible. Technical names (field names, metric names,
code, product names) are permitted as they are. Technical verbs from the
domain (ingest, query, select, aggregate, group, upsert, deploy, commit) are
permitted.

### Sentences

- **Procedural sentences: 20 words maximum. Descriptive sentences: 25 words
  maximum.** Count the words. Code in backticks counts as one word.
- **One instruction per sentence.** Two actions at the same time are the only
  exception.
- **One topic per sentence.** Do not join two facts with a dash, a semicolon
  or "which".
- **Paragraphs: one topic, six sentences maximum.**
- **Use the active voice.** Not "the batch is rejected". Write "tidepool
  rejects the batch".
- **Use the imperative for instructions.** "Set `epoch_interval` to `1s`."
  Not "You may want to set…".
- **Use only simple tenses**: present, simple past, simple future.
- **Do not use the -ing form** as a verb or a noun ("when using", "grouping
  helps"). Write "when you use", "a group helps". Technical names are an
  exception (`ignore_selection`, "load balancing" as a term).
- **Use articles** ("the", "a") and do not omit words to make text short.
- **Use vertical lists** for sequential steps and for a set of items.
- **Write warnings and cautions with the command first**, then the reason:
  "Do not remove a table that holds data without a backup. The change deletes
  its rows."
- **Use "must" for a requirement, "can" for a possibility, "do not" for a
  prohibition.** Avoid "should", "might", "may", "would", "could".
- **No contractions** (don't, it's, isn't, you'll).
- **Noun clusters: three words maximum.** Not "dashboard filter list window
  size". Write "the window size of a filter list".

### Words

One word has one meaning. Use the same word for the same thing on every page.

| do not write | write |
| --- | --- |
| ensure | make sure |
| utilize, leverage | use |
| perform, carry out | do |
| approximately, roughly | about |
| prior to | before |
| subsequent, following (as "after") | after |
| in order to | to |
| via | through, with |
| allows you to, lets you | you can |
| numerous, a number of | many |
| obtain | get |
| commence, kick off | start |
| terminate | stop |
| emit (in prose) | send |
| take down, bring up, set up (phrasal verbs) | stop, start, configure |
| e.g., i.e., etc. | for example, that is (or rewrite) |

Terms of tidepool. Use them with this meaning only:

| term | meaning |
| --- | --- |
| table | rows that tidepool stores, declared in `tables/<name>.yaml` |
| dataset | the model's view of a table, with its fields |
| field | `dataset.field`, a column or a derived expression |
| metric | an aggregate (`SUM(orders.amount)`) or a combination of metrics |
| selection | the values selected on a dashboard, over all its fields |
| epoch | one published state of the data. New rows are visible from the next epoch. |
| change | a set of config files to set or remove, planned and applied as one |
| visual | a KPI, bar, line or table on a dashboard. Not "widget", not "chart" for all four. |
| filter | a list, tree or date range on the rail of a dashboard |

### Things to remove

- Rhetorical contrast: "not X, but Y", "X, not Y", "rather than" in every
  paragraph. State what it is.
- Design-justification in user text: "deliberately", "on purpose", "that is
  the point", "the honest answer", "load-bearing", "this is what makes",
  "worth knowing", "the one thing". User text says what tidepool does and
  what to do. The design reasons belong in `CLAUDE.md` and `docs/plan.md`.
  Keep a reason only when the user needs it to make a correct decision, and
  then give it in one sentence.
- Em dashes as joints between clauses. Use a full stop.
- Clever asides, metaphors, personification. No puns on tides, pools, waves
  or the sea.
- Marketing words: effortless, seamless, powerful, blazing, unleash, simply,
  just, easily, robust, real-time (write "live", and give the number).
- Hedges and filler: "really", "actually", "quite", "a little", "of course",
  "in fact", "basically".
- Second-guessing the reader: "you might think", "it is tempting to".
- Exclamation marks.

### Format

- The name is **tidepool**, lowercase, also at the start of a sentence. The
  same for **kayak**. Apache Ossie, Qlik, DuckDB and Prometheus keep their own
  spelling.
- Headings stay lowercase. Body text uses normal sentence capitalization.
- Field names, metric names and config keys in backticks: `customers.region`,
  `revenue`, `epoch_interval`.
- Units: "ms", "s", "MB", "GB". Numbers with digits.

### Example

Before:

> Edits to the files that aren't safe wait under Config → Waiting for
> approval; a server started on a directory that changed while it was down
> applies the difference at startup.

After:

> tidepool applies a safe change from the files at once. A change of another
> class waits for approval on the config page. When the server starts, it
> applies the changes to the files since it stopped.

## 3. Technical constraints (for whoever edits)

- VitePress compiles the pages as Vue. A bare `<name>` or `{{` in text or in
  a doc comment is a build error. Put it in backticks.
- If you change a heading that other pages link to, keep the old anchor with
  VitePress syntax: `## new heading {#old-slug}`.
- Do not change a statement of fact unless you verified it in the code. Copy
  must be correct before it is short.
- The pages under `website/reference/generated/` are generated by
  `just docs` from the doc comments. Do not edit them by hand.
- Every YAML sample on the site that holds a whole config file is compiled by
  `crates/tidepool-docsgen/tests/site.rs`. Mark a fragment that is not a whole
  file with `yaml` and a comment line `# fragment` at the top.
