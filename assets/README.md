<div align="center">

<img alt="Auto Ontology MCP: a semantic layer your AI assistant builds and queries. Works with Claude, Codex, OpenClaw, Hermes and any MCP client." src="docs/assets/banner.png" width="100%">

<p>
  <a href="https://github.com/gogoymh/auto-ontology-mcp/stargazers"><img src="https://img.shields.io/github/stars/gogoymh/auto-ontology-mcp?style=social" alt="GitHub stars"></a>
  <a href="https://github.com/gogoymh/auto-ontology-mcp/actions/workflows/ci.yml"><img src="https://github.com/gogoymh/auto-ontology-mcp/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-blue" alt="License: Apache-2.0"></a>
  <img src="https://img.shields.io/badge/python-3.12%2B-3776AB?logo=python&logoColor=white" alt="Python 3.12+">
  <a href="https://modelcontextprotocol.io"><img src="https://img.shields.io/badge/MCP-stdio%20%7C%20streamable%20HTTP-8250df" alt="MCP: stdio and streamable HTTP"></a>
</p>

<p>
  <a href="#quick-start">Quick start</a> ·
  <a href="#see-it-in-action">Demo</a> ·
  <a href="#personal-knowledge-base">Knowledge base</a> ·
  <a href="#tools">Tools</a> ·
  <a href="#how-it-works">How it works</a> ·
  <a href="#safety">Safety</a>
</p>

<a href="https://github.com/gogoymh/auto-ontology-mcp"><img src="docs/assets/star.gif" width="380" alt="Animation: a cursor clicks the Star button and it turns into Starred"></a>

</div>

**Auto Ontology MCP** (`auto-ontology-mcp`) brings [NVIDIA Auto Ontology](https://github.com/NVIDIA/auto-ontology)'s semantic layer to any MCP client, for two jobs:

- **Ask your databases.** Point it at a database, and the model you already use (Claude Code, Codex, OpenClaw, Hermes Agent, Cursor, VS Code, or the Claude and ChatGPT apps) builds business Terms, reviews joins and answers questions with SQL that the server checks against the ontology and the data.
- **Keep a personal knowledge base.** Share receipts, statements, papers, book notes or screenshots with your assistant; it reads them and stores the facts as records, the ideas as linked concepts, the passages worth finding again in a full-text index, and where each came from. The full originals stay in your Obsidian vault. Later questions get exact numbers, searchable quotes and links back to the original.

> [!TIP]
> **Use it in place of an LLM Wiki.** In Karpathy's [LLM Wiki](https://gist.github.com/karpathy/442a6bf555914893e9891c11519de94f) pattern the model compiles your sources into markdown pages that you read in Obsidian. Auto Ontology MCP runs the same loop (ingest, query, lint, with an index and a log) on an ontology instead of loose pages: facts go into typed tables you can sum, filter and compare with SQL, ideas become concept pages with typed links, every record, page and link keeps its source and the quote behind it, and `export_wiki` writes it all out as an Obsidian vault with aliases, `[[links]]` and the graph view. [How the two map](#personal-knowledge-base)

Auto Ontology runs its own LLMs (NVIDIA NIM) behind a Next.js, FastAPI and Postgres stack. This project hands every language step (reading, extracting, naming, judging, writing SQL) to the model in your MCP client, so there is no model endpoint to host and no API key to configure. The server keeps the deterministic work: schema extraction, profiling, key inference, validation, storage and read-only execution, in local SQLite files.

## Why

Text-to-SQL fails quietly. The query runs, a number comes back, and nothing says it is wrong.

| ❌ A model reading a raw schema | ✅ The same model with auto-ontology-mcp |
| --- | --- |
| Guesses which of `billing_city_id` and `delivery_city_id` "the customer's state" means | Gets the join path from reviewed relationships, with the default role and the alternatives spelled out |
| Joins `customers.customer_id = orders.order_id` because the integers happen to overlap | `validate_sql` rejects the join and names the one the ontology knows |
| Filters `state_name = 'pennsylvania'` and reports zero revenue | Is told the stored value is `'Pennsylvania'` before running the query |
| Writes `discount_pct > 20` against a column that holds 0 to 0.3 | Is asked whether it meant `0.2` |
| Sums a product price after joining to order lines and inflates the total | Gets a fan-out warning for the one-to-many join |
| Sums a product's order lines and counts its tags in one SELECT, so each line is counted once per tag | Is told that order lines and tags meet only at the product, and to aggregate each in its own subquery |
| Links two tables because both point to the same parent (an order line and a product tag of the same product) | Hears from `find_join_path` and `check_answerable` that the path only meets at the product and does not say which row belongs to which |
| Answers "return on ad spend" with whatever revenue column it finds | Hears from `check_answerable` that nothing covers ad spend, and says so |

## See it in action

`auto-ontology-mcp demo` sets up three demos about one fictional store: a synthetic retail database (11 tables, 3,200 orders), three of the store's documents in the knowledge store (a meeting note, a tag guide and a quality digest, with their originals in the Obsidian vault), and questions that need both. Tool output below is abridged from real runs of the test fixtures (`tests/test_demo.py`); the model's replies are illustrative.

### 1. Structured data: the retail database

A question asked after the model modeled the tables and reviewed the joins. In that review it made `customers.delivery_city_id` the default city role, so states below are delivery states. A fresh scan makes `billing_city_id` the default instead: neither column name is the unqualified `city_id`, and Auto Ontology's fallback then picks the alphabetically first role. `set_relationship(..., is_default=true)` changes it, and `get_query_context` names the other role in its role notes either way.

```text
You     What was revenue for the sushi roll flash drive in Pennsylvania in June 2024?

Model   → check_answerable(entities=["revenue", "sushi roll flash drive", "Pennsylvania"])
          verdict: answerable
          revenue                  covered           Order Line › Revenue (order_lines.line_total)
          sushi roll flash drive   covered_by_value  products.product_name = 'USB Food Flash Drive - Sushi Roll'
          Pennsylvania             covered_by_value  states.state_name = 'Pennsylvania'

        → get_query_context(terms=["Order Line", "Product", "State", "Order"])
          join path: order_lines → products
                     order_lines → orders → customers → cities → states
          role note: customers.delivery_city_id = cities.city_id is the default;
                     other roles: customers.billing_city_id (billing)

        → validate_sql(first draft)
          ✗ Join customers.customer_id = orders.order_id is not how the ontology joins these
            tables (orders.customer_id = customers.customer_id).
          ! No row has states.state_name = 'pennsylvania'. Close real values: 'Pennsylvania'.

        → run_sql(fixed query)
          revenue 316.8, order_lines 4
          draft Terms used: City, Customer, Order, Order Line, Product, State

Model   Revenue was 316.80 from 4 order lines (delivery state Pennsylvania, June 2024).
        The Terms behind this answer are still drafts, so treat the definition as unreviewed.
        [SQL shown]
```

And a question the data cannot answer:

```text
You     What was return on ad spend for the sushi roll flash drive?

Model   → check_answerable(...)
          verdict: partially_answerable   coverage: 0.33
          not covered: "return on ad spend", "ad spend"

Model   This database has sales but no advertising spend, so I can't compute ROAS.
        I can give you revenue for that product instead.
```

### 2. Unstructured data: the store's documents

The demo stores the three documents the way a model would after reading them: each full text as an original note in the vault, the passages worth finding again, the concepts and causal links they describe, and the action items they decide.

```text
You     [Merchandising tag guide] Remember this.

Model   → save_passages(source={title: "Merchandising tag guide", kind: "wiki", original_text: "# Merchandising tag guide …"},
              passages=[{heading: "bestseller", text: "A product the buyers want to feature …", terms: ["Bestseller Tag"]}, …])
          original kept in the vault: originals/Merchandising tag guide.md
        → save_concept("Bestseller Tag", relations=[is_a Merchandising Tag], source=2, locator="bestseller")

You     What does the bestseller tag mean?

Model   → search_text("bestseller")
          term     Bestseller Tag                          [Bestseller] Tag
          passage  Merchandising tag guide › bestseller    "A product the buyers want to feature on the home page.
                                                            It is a placement decision and does not mean the
                                                            product sells the most units."
                   link: [[originals/Merchandising tag guide#bestseller]]

Model   Buyers pick it for home-page placement; it is not a sales ranking
        ([[originals/Merchandising tag guide#bestseller]]).

You     What led to the Midwest sales decline?

Model   → run_sql(datasource="knowledge", sql="WITH RECURSIVE chain AS (… FROM term_relations r JOIN chain
              ON r.to_term_id = chain.id WHERE r.relation IN ('causes', 'part_of')) …")
          Fewer Repeat Orders   part_of   1
          Delivery Delay        causes    2
          September Stockout    causes    2
          Fulfilment Cutover    causes    3

You     Which product problems sent products back?

Model   → run_sql(datasource="knowledge", sql="SELECT t.name, p.heading, snippet(passages_fts, …)
              FROM passages_fts JOIN passages p … JOIN passage_terms … JOIN terms t … JOIN term_relations r …
              WHERE passages_fts MATCH '"returns"* OR "back"' AND r.relation = 'instance_of'
              AND r.to_term = 'Product Quality Issue' ORDER BY bm25(passages_fts)")
          Wobbling Chair Base    Office Chair                  …Several chairs came [back] before support …
          Ear Cushion Cracking   Noise Cancelling Headphones   …the product customers sent [back] most often.
```

The last query is a full-text search, a walk over typed relations and a join to the passages, in one SQL statement that `validate_sql` checks like any other.

### 3. Structured and unstructured together

The database says what happened; the documents say why. The model queries both datasources and keeps the numbers and the explanations apart.

```text
You     Why did Midwest revenue fall in 2024?

Model   → run_sql(datasource="demo", sql="SELECT substr(o.order_date, 6, 2) AS month, … WHERE r.region_name = 'Midwest' …")
          month   orders_2023   orders_2024
          01      32            17
          02      30            19
          09      34            16
        → search_text("Midwest January September", kinds=["passage"])
          Midwest sales review, Q3 2024 › Summary   "Midwest orders fell behind last year twice in 2024:
                                                     in [January] and February, and again in [September]. …"
        → run_sql(datasource="knowledge", sql="WITH RECURSIVE … term_relations …")
          Fulfilment Cutover → Delivery Delay → Fewer Repeat Orders ← September Stockout

Model   Midwest orders dropped to 17 and 19 in January and February 2024 (32 and 30 a year earlier) and to 16 in
        September (34). The Q3 sales review gives the reasons: fulfilment moved to Lakeshore Logistics on January 8
        and deliveries took eight days instead of three, then Lakeshore ran out of desks and chairs in early September
        ([[originals/Midwest sales review, Q3 2024#January cutover]], [[…#September stockout]]).

You     Which open action items deal with that, and who owns them?

Model   → run_sql(datasource="knowledge", sql="SELECT a.title, a.owner, a.due_on, group_concat(DISTINCT p.heading)
              FROM action_items a JOIN sources s ON s.id = a._source_id JOIN passages p ON p.source_id = s.id
              JOIN passages_fts ON passages_fts.passage_id = p.id
              WHERE a.status = 'open' AND passages_fts MATCH '"stockout"* OR "delivery"*' GROUP BY …")
          Move Ohio and Michigan fulfilment back to Columbus   Min Lee         2025-01-31   January cutover, September stockout
          Send a win-back offer to Midwest customers           Dana Whitfield  (none)       January cutover, September stockout

You     Do bestseller products sell more?

Model   → run_sql(datasource="demo", …)    bestseller: 7 products, 1,309 units each on average; other: 11 products, 1,365
        → search_text("bestseller")         "It is a placement decision and does not mean the product sells the most units."

Model   No: bestseller products sold slightly fewer units on average (1,309 against 1,365). That fits the tag guide,
        which says buyers assign the tag for home-page placement rather than from sales.
```

Remove everything the demo added to the knowledge store and the vault with `auto-ontology-mcp demo --remove`; `--no-knowledge` sets up only the database.

## What it does

| | |
| --- | --- |
| **Catalog and profiling** | Reads tables, columns, declared keys and comments through SQLAlchemy, then samples rows to record top values, uniqueness, null rates, ranges, date formats, the complete value set of small text enums, and text columns that store lists (`'SLE, RA'`) |
| **Key inference** | Finds undeclared primary and foreign keys by name and confirms them by value containment. Detects roles (`billing_` vs `delivery_`), self references and bridge tables, and lists identifier columns it could not place |
| **Modeling with your model** | Hands the model one table at a time with samples, keys and the modeling rules from Auto Ontology, then validates and stores the Terms and column mappings it proposes |
| **SQL attributes** | Mines reusable filters, measures and join patterns from executed and verified queries and bridge tables, runs each candidate against the database before saving it |
| **Answerability** | Matches the question's entities to Terms, attributes and SQL attributes, and to names stored in the data (products, places, statuses). As in Auto Ontology, the glossary interprets a phrase rather than covering it: a phrase that matches a glossary entry is covered when the entry's definition names or matches part of the model. It then checks how the matched tables link: row to row, only at a table the question also names (counted per that table), only at a shared parent it does not name, or not at all. Join paths prefer a route that ties rows to rows over a shorter one that only meets at a shared parent |
| **Validation** | Unknown columns (with the ontology's name for them), joins the ontology does not know (a join saved as a join SQL attribute counts as known), near-miss literals (in `WHERE`, `HAVING` and comparisons in the select list such as `SUM(status = 'shipped')`), `=` against a column that stores lists, percent-vs-fraction thresholds (`run_sql` checks these too; given the question, a percentage it states is checked even when the number fits the column, so `rate < 0.18` for "below 0.18%" against a 0 to 1 column is flagged), one-to-many fan-out, and a dry run. Fan-out is judged on what is aggregated: `SUM(products.unit_price)` over order lines is flagged, while `SUM(order_lines.quantity * products.unit_cost)` or a `CASE WHEN orders.… THEN order_lines.line_total END` is not, because the value is computed per order line. Two branches that hang one-to-many from the same table (order lines and product tags of a product) are flagged when aggregated together, unless the query groups by the other branch (revenue by tag). Four checks need only the parsed query: integer division where it truncates (SQLite, PostgreSQL), a constant-false `WHERE` or an all-`NULL` select list, a `GROUP BY` on the primary key of the only table read, and a many-side column selected bare next to a `GROUP BY` on the one side. On PostgreSQL an unquoted mixed-case name is reported with the query rewritten in quotes |
| **Governance** | Draft, certified and deprecated status on Terms and SQL attributes; editing a certified definition returns it to draft. Verified question and SQL pairs feed later answers |
| **Knowledge store** | A writable store for what your assistant reads: entities with typed columns and references, concepts with a markdown body and typed relations, passages (excerpts and section summaries), and a source with locator and quote behind every record. An index, a change log and an Obsidian export make it usable as an LLM Wiki |
| **Full-text search** | FTS5 indexes over passages, Term bodies and sources, ranked with bm25. Prefix matching finds words with Korean particles attached (기억 → 기억을), and a substring pass finds them inside compounds. `search_text` returns snippets, headings and `[[links]]` to the originals |
| **Concept graph in SQL** | Terms, typed relations, Term sources and passages are tables next to the records, and the FTS indexes are queryable too, so one validated query can combine a text search, a `WITH RECURSIVE` walk over relations and the records |
| **Originals in Obsidian** | The full text of each source stays in your Obsidian vault: a note already there, transcribed text saved as a new note, or a copied PDF or image. The store keeps the summary and excerpts and links back to the heading they came from |
| **Updates in place** | Nothing is rebuilt when a source changes. A re-scan keeps Terms, certification and reviewed joins, and sends new tables and columns added to modeled tables back to the model. A changed document replaces its own passages and gets its new original as a new note next to the old one |
| **Portable** | Export and import the whole semantic layer as one YAML or JSON document |

Results of the scan on the demo database, as reported by `auto-ontology-mcp demo`:

| Undeclared foreign keys in the demo | Found and auto-accepted | Proposed for review | Reported as unlinked |
| :---: | :---: | :---: | :---: |
| 9 | 7 | 1 (`employees.manager_id`, a self reference) | 1 (`orders.sales_rep_id`, no name match) |

## Quick start

You need [uv](https://docs.astral.sh/uv/getting-started/installation/).

**1. Set up the demos** (or skip to [your own database](#connect-your-database)). This creates the retail database and stores three demo documents in the knowledge store, with their originals in `~/.auto-ontology-mcp/wiki/knowledge/originals`:

```bash
uvx --from git+https://github.com/gogoymh/auto-ontology-mcp auto-ontology-mcp demo
```

`uvx` caches what it installs from GitHub. To move to a newer version, run any command once with `--refresh`, for example `uvx --refresh --from git+https://github.com/gogoymh/auto-ontology-mcp auto-ontology-mcp --version`, then restart your client. A store created by an earlier version is upgraded the first time the new version opens it.

**2. Connect your client.** One click for Cursor or VS Code (both need [uv](https://docs.astral.sh/uv/getting-started/installation/) installed):

<p>
  <a href="https://cursor.com/en/install-mcp?name=auto-ontology&config=eyJjb21tYW5kIjoidXZ4IiwiYXJncyI6WyItLWZyb20iLCJnaXQraHR0cHM6Ly9naXRodWIuY29tL2dvZ295bWgvYXV0by1vbnRvbG9neS1tY3AiLCJhdXRvLW9udG9sb2d5LW1jcCJdfQ%3D%3D"><img src="https://cursor.com/deeplink/mcp-install-dark.svg" alt="Install in Cursor" height="32"></a>
  <a href="https://insiders.vscode.dev/redirect?url=vscode%3Amcp%2Finstall%3F%257B%2522name%2522%253A%2522auto-ontology%2522%252C%2522command%2522%253A%2522uvx%2522%252C%2522args%2522%253A%255B%2522--from%2522%252C%2522git%252Bhttps%253A%252F%252Fgithub.com%252Fgogoymh%252Fauto-ontology-mcp%2522%252C%2522auto-ontology-mcp%2522%255D%257D"><img src="https://img.shields.io/badge/VS_Code-Install_Server-0098FF?style=for-the-badge&logo=visualstudiocode&logoColor=white" alt="Install in VS Code" height="32"></a>
</p>

For the other clients, copy the snippet below. `auto-ontology-mcp config` prints them with absolute paths filled in, which desktop apps need when `uvx` is not on their `PATH`.

<details open>
<summary><b>Claude Code</b></summary>

```bash
claude mcp add auto-ontology -- uvx --from git+https://github.com/gogoymh/auto-ontology-mcp auto-ontology-mcp
```
</details>

<details>
<summary><b>Codex CLI</b></summary>

Add to `~/.codex/config.toml`:

```toml
[mcp_servers.auto-ontology]
command = "uvx"
args = ["--from", "git+https://github.com/gogoymh/auto-ontology-mcp", "auto-ontology-mcp"]
```
</details>

<details>
<summary><b>OpenClaw</b></summary>

Save the server in OpenClaw's MCP registry ([docs](https://docs.openclaw.ai/cli/mcp/registry)). OpenClaw asks for the local Gateway to be stopped while saved servers change, so start it again afterwards:

```bash
openclaw mcp set auto-ontology '{"command":"uvx","args":["--from","git+https://github.com/gogoymh/auto-ontology-mcp","auto-ontology-mcp"]}'
openclaw mcp list
```

The tools show up in OpenClaw's normal coding and messaging tool profiles.
</details>

<details>
<summary><b>Hermes Agent</b></summary>

Add to `~/.hermes/config.yaml` ([docs](https://hermes-agent.nousresearch.com/docs/user-guide/features/mcp)), then run `/reload-mcp` in a session:

```yaml
mcp_servers:
  auto_ontology:
    command: "uvx"
    args: ["--from", "git+https://github.com/gogoymh/auto-ontology-mcp", "auto-ontology-mcp"]
```

Tools are registered as `mcp_auto_ontology_<tool>`, for example `mcp_auto_ontology_run_sql`. Hermes passes stdio servers only a baseline environment, so if you keep the store somewhere other than `~/.auto-ontology-mcp`, set it under `env:` (`AUTO_ONTOLOGY_HOME: /path`).
</details>

<details>
<summary><b>Cursor</b></summary>

Add to `~/.cursor/mcp.json`:

```json
{
  "mcpServers": {
    "auto-ontology": {
      "command": "uvx",
      "args": ["--from", "git+https://github.com/gogoymh/auto-ontology-mcp", "auto-ontology-mcp"]
    }
  }
}
```
</details>

<details>
<summary><b>VS Code</b></summary>

Add to `.vscode/mcp.json`:

```json
{
  "servers": {
    "auto-ontology": {
      "type": "stdio",
      "command": "uvx",
      "args": ["--from", "git+https://github.com/gogoymh/auto-ontology-mcp", "auto-ontology-mcp"]
    }
  }
}
```
</details>

<details>
<summary><b>Claude Desktop</b></summary>

Add to `claude_desktop_config.json` and restart Claude Desktop:

```json
{
  "mcpServers": {
    "auto-ontology": {
      "command": "uvx",
      "args": ["--from", "git+https://github.com/gogoymh/auto-ontology-mcp", "auto-ontology-mcp"]
    }
  }
}
```
</details>

<details>
<summary><b>ChatGPT and claude.ai (remote)</b></summary>

Web clients only reach MCP servers on a public HTTPS URL. Run the HTTP transport with a token and expose it through a tunnel:

```bash
uvx --from git+https://github.com/gogoymh/auto-ontology-mcp auto-ontology-mcp \
  serve --transport http --port 8765 --token "$(openssl rand -hex 24)"
cloudflared tunnel --url http://127.0.0.1:8765      # or: ngrok http 8765
```

Add `https://<tunnel-host>/<token>/mcp` as a custom connector with no authentication. Connector settings in these apps take a URL but no custom headers, so the token can sit in the path. Clients that do send headers can use `Authorization: Bearer <token>` or `?key=<token>`. In ChatGPT, write tools need a connector added in developer mode; menu names change between releases.

The `search` and `fetch` tools follow the shape ChatGPT expects from connectors used for search and deep research.
</details>

**3. Ask.** Try these in order:

1. *"Use auto-ontology to build the ontology for the demo datasource: review the proposed joins, model every table, then save useful SQL attributes."* (or pick the `build_ontology` prompt)
2. *"Which states brought in the most delivered revenue in 2024?"*
3. *"What was return on ad spend last quarter?"* The answer should be that the data cannot say.
4. *"Why did Midwest revenue fall in 2024?"* The numbers come from the database, the reasons from the demo documents.
5. Attach a receipt, a statement, an article or your notes on a book and say *"Remember this."* (or pick the `remember` prompt). See [Personal knowledge base](#personal-knowledge-base).
6. *"Export my knowledge base for Obsidian."* `export_wiki` writes pages next to the originals in `~/.auto-ontology-mcp/wiki/knowledge`; open that folder in Obsidian as a vault.

## Connect your database

```bash
uvx --from git+https://github.com/gogoymh/auto-ontology-mcp --with "psycopg[binary]" auto-ontology-mcp \
  add shop "postgresql+psycopg://readonly_user:pass@host:5432/db" --schemas sales,geo
```

Add the same `--with` to the client configuration's `args` so the server can load the driver.

You can also give the model a URL and let it call `add_datasource`. Any SQLAlchemy URL works:

| Database | URL | Driver (`uvx --with`) |
| --- | --- | --- |
| SQLite | `sqlite:////absolute/path/file.db` | built in |
| PostgreSQL | `postgresql+psycopg://user:pass@host:5432/db` | `psycopg[binary]` |
| MySQL / MariaDB | `mysql+pymysql://user:pass@host:3306/db` | `pymysql` |
| DuckDB | `duckdb:////absolute/path/file.duckdb` | `duckdb-engine` |
| Oracle | `oracle+oracledb://user:pass@host:1521/?service_name=ORCLPDB1` | `oracledb` |

From a clone, `uv sync --extra postgres` (or `mysql`, `duckdb`, `oracle`, `all`) installs the same drivers.

The test suite runs against SQLite and PostgreSQL 17, and against Oracle when `AO_TEST_ORACLE_URL` is set. MySQL and DuckDB go through the same SQLAlchemy and sqlglot code paths but have not been tested against a live server yet.

### Large schemas

By default a scan reads every table and view. A schema with thousands of tables takes hours to scan and profile, so give the scan a table filter:

```bash
export AUTO_ONTOLOGY_SCAN_TABLES="STJ_KTP_*,ACB_*,!*_H"      # default filter for every scan
auto-ontology-mcp add vit "oracle+oracledb://..." --tables "STJ_KTP_*"   # saved with the datasource
auto-ontology-mcp scan vit --tables "ADR_*"                      # this scan only
```

The same patterns work as `tables=[...]` on the `add_datasource` and `scan_datasource` tools.
- **Syntax.** Patterns are case-insensitive globs: `*` and `?`, with `%` working like `*`. `_` is literal. A pattern with a dot matches `schema.table` (`HR.EMP*`). `!pattern` excludes, and exclusions alone mean "everything except".
- **Precedence.** The `tables` argument of the call wins, then the datasource's saved filter, then `AUTO_ONTOLOGY_SCAN_TABLES`, then all tables. `*` reads everything.
- **Growing the catalog.** Tables scanned earlier that fall outside the filter are kept, with their Terms and relationships, so you can grow the catalog area by area. A scan removes a table only when the filter covers it and the database no longer has it.
- **What each scan touches.** Only the tables a scan reads are profiled. Join inference runs across the whole catalog.
- **Knowledge store.** The built-in `knowledge` store is never filtered.

**Oracle notes.**
- Oracle Database 12.1 or later is required. Row limits are written as `FETCH FIRST n ROWS ONLY`, which 11g does not understand, so 11g is not supported in either driver mode.
- `python-oracledb` runs in thin mode by default, which needs no Oracle client.
- For servers that enforce native network encryption, install Oracle Instant Client and set `AUTO_ONTOLOGY_ORACLE_CLIENT_LIB` to its directory (or to `1` if it is on the library path). This switches the driver to thick mode.
- A scan covers the login user's schema. Pass `--schemas HR,SALES` (or `schemas=[...]`) to scan other owners'.
- Application schemas often hold thousands of tables. Scan only the areas you model with a table filter (see [Large schemas](#large-schemas)).
- The catalog keeps Oracle's own spelling (`CUSTOMERS`, `CUSTOMER_ID`), so unquoted lowercase SQL works as it does in SQL*Plus.

## Personal knowledge base

Your assistant can already read PDFs, photos, web pages and notes. What it lacks is a place to keep what it read, in a form it can count, compare and cite later. The knowledge store is that place, built from the same parts as Auto Ontology's semantic layer:

| Part | What it holds | Example |
| --- | --- | --- |
| **Entity** | Facts you will count, sum, filter or compare: a Term with its own table, typed columns and references to other entities | `Receipt`: paid_on, merchant → `Merchant`, category, total_krw |
| **Concept** | Meaning: a one-line definition, a markdown body and typed relations (`is_a`, `part_of`, `related_to`, `causes`, ...) | `Spaced Repetition` is_a `Learning Technique`, related_to `Forgetting Curve` |
| **Passage** | An excerpt or a section summary worth finding again, with its heading or locator in the original and the Terms it is about; full-text indexed | *Make It Stick*, ch. 2: "Retrieval practice beats rereading." about `Retrieval Practice` |
| **Source** | Where each record, concept, relation and passage came from, with a locator and the quote behind it, and the path of the full original in the Obsidian vault | `IMG_2041.jpg`, line 3; `originals/Make It Stick ch2.md` |

The store is created the first time the model saves something (datasource `knowledge`, file `~/.auto-ontology-mcp/knowledge.sqlite`). Everything else in this README applies to it as well: synonyms in your own language, the glossary, SQL attributes for definitions like "eating out", validation, certification and verified example queries.

### Originals in Obsidian, summaries and excerpts in the store

A long document does not belong in a database row, and an excerpt is no use without the page around it. So the full original stays in your Obsidian vault, where you read and annotate it, and the store keeps what you search and count: a summary on the source, passages for the parts worth finding again, records for the facts and concepts for the ideas. A source points at its original in one of three ways:

| Source field | When | What happens |
| --- | --- | --- |
| `original` | The note is already in your vault (a web clipping, your own notes) | The path is checked and stored, relative to the vault |
| `original_text` | The text exists only in the conversation (a web page the model read, a transcript) | Written as a new note in `originals/`, with the source's title, URI, date and a hash of the text as frontmatter |
| `original_file` | A local PDF, image or document | Copied into `originals/`. Hidden files and other file types are refused |

Existing files are never overwritten; a different original with the same name gets a numbered name (see [When a source changes](#when-a-source-changes)). Passages carry the heading they came from, so search results and exported pages link to `[[originals/Note#Heading]]` and Obsidian opens the original at that section. Set `AUTO_ONTOLOGY_VAULT` to your own vault and the generated pages and originals go to its `auto-ontology/` folder; otherwise `~/.auto-ontology-mcp/wiki/knowledge` is the vault.

### When a source changes

Nothing is rebuilt. Each change touches the rows it concerns, and the search indexes follow row by row.

| What changed | What the model does | What happens |
| --- | --- | --- |
| A document | Saves the source again with the same `uri` (or its id) and the new `original_text` or `original_file`, then calls `save_passages(..., replace=true)` with every passage of the new version | The source keeps its id. Passages whose text is unchanged keep their ids and Term links; the others are deleted and listed in `deleted`. The new original becomes a new note (`Team handbook (2).md`) and the source points at it, so passage `[[links]]` open the new version. The old note stays with your annotations, and `recent_changes` records the switch. `get_source` lists the records and concepts that cite the source, to correct with `edit_records` and `save_concept` |
| A database schema | Calls `scan_datasource` (or you run `auto-ontology-mcp scan NAME`) | Tables and columns are added to and dropped from the catalog. Terms, certified status, reviewed joins and SQL attributes stay, and inferred joins are tested again. `get_modeling_worklist` returns new tables and modeled tables that gained columns; the latter come with `new_columns` and the `current_terms` to send back with the new columns placed |
| Rows in a database | Nothing | Queries run against the live database. A re-scan refreshes samples and profiles |

The server does not watch files or compare document versions on its own: the model reads the new version and tells the store what changed. Saving the same document again changes nothing. Notes written by 0.5.0 and later carry a `sha256` of the text in their frontmatter, so a note you have annotated in Obsidian still counts as the same original; older notes are compared by their text. A note you linked yourself with `original` is yours to update and is never replaced.

### Searching and combining

`search_text` searches passages, Term definitions and bodies, and sources with SQLite FTS5 (bm25 ranking), and returns snippets, headings, the Terms each passage is about and a link to the original. Words match with their Korean particles (기억 finds 기억을) and inside compounds (곡선 finds 망각곡선을).

The same data is in tables next to your records, so SQL can combine them: `terms`, `term_relations` (walk chains with `WITH RECURSIVE`), `term_sources`, `passages`, `passage_terms`, and the indexes `passages_fts`, `terms_fts` and `sources_fts`:

```sql
-- Passages that mention 간격 and are about a concept that is_a Learning Technique
SELECT s.title, p.heading, snippet(passages_fts, 3, '[', ']', '…', 12) AS hit
FROM passages_fts
JOIN passages p ON p.id = passages_fts.passage_id
JOIN sources s ON s.id = p.source_id
JOIN passage_terms pt ON pt.passage_id = p.id
JOIN terms t ON t.id = pt.term_id
JOIN term_relations r ON r.from_term_id = t.id
WHERE passages_fts MATCH '"간격"*' AND r.relation = 'is_a' AND r.to_term = 'Learning Technique'
ORDER BY bm25(passages_fts);
```

`validate_sql` knows these tables, their joins and the FTS functions, so these queries get the same checks as any other. The Term tables mirror the ontology and are read-only; change Terms through `save_concept`, `define_entity` and `update_term`.

The indexes follow each write row by row: triggers reindex the passages and sources that changed, and only Terms that changed are copied to the mirror. A write therefore costs about the same in a large store as in a small one. With 20,000 passages, adding a record took about 20 ms and saving a passage about 40 ms (Apple M4 Pro, median of five writes).

### A session

The tool calls and results below are the ones in [`tests/test_knowledge.py`](tests/test_knowledge.py); what the user and the model say is illustrative.

```text
You     [two receipt photos] Remember these.

Model   → search_ontology("receipt", datasource="knowledge")              no match yet
        → define_entity("Merchant", columns=[name (key), city])
        → define_entity("Receipt", table="receipts", synonyms=["영수증"],
              columns=[paid_on date required, merchant ref:Merchant, category, total_krw integer])
        → add_records("Receipt", source={"title": "IMG_2041.jpg", "kind": "image"}, records=[
              {"paid_on": "2026/10/03", "merchant": "을지로 국수집", "category": "Eating Out",
               "total_krw": "12,000원", "_evidence": "합계 12,000"}, ...])
          inserted 3 · created Merchant: 을지로 국수집, Chipotle
          (dates and amounts normalized; adding the same receipt again changes nothing)

You     [book notes] Keep my notes on chapter 2 of Make It Stick.

Model   → save_concept("Spaced Repetition", description="Reviewing material at increasing intervals.",
              body="...", relations=[is_a Learning Technique, related_to Forgetting Curve],
              source={"title": "Make It Stick", "kind": "book"}, locator="ch. 2")
          empty concepts created to fill in later: Learning Technique, Forgetting Curve

You     How much did I spend eating out in October, and where is that from?

Model   → check_answerable(["영수증", "Eating Out"], datasource="knowledge")    answerable
        → run_sql("SELECT SUM(total_krw) FROM receipts
                   WHERE category = 'Eating Out' AND paid_on LIKE '2026-10-%'")   27500
        → get_source(1)      IMG_2041.jpg: 3 Receipt and 2 Merchant records

Model   27,500 KRW, from two receipts in IMG_2041.jpg.
```

If a record in a batch is wrong (a date written as "last tuesday", an amount as "12%", an unknown column), nothing in the batch is saved and every problem is listed, so the model fixes and retries. Records that other records reference cannot be deleted, an entity with records is only dropped with `force`, and column types cannot change.

### Using it as an LLM Wiki

Every part of the [LLM Wiki](https://gist.github.com/karpathy/442a6bf555914893e9891c11519de94f) pattern has a counterpart here:

| LLM Wiki | Auto Ontology MCP |
| --- | --- |
| Raw sources the model reads but never edits | Originals in the Obsidian vault, and sources in the store: title, path or URL, summary and the vault path of the original, linked from every record, concept, relation and passage |
| Wiki pages the model writes | Concepts (definition, markdown body, typed relations) and entities (records with typed columns) |
| Schema file (`CLAUDE.md`, `AGENTS.md`) | Rules the server sends with its handshake and tools, plus the `remember` prompt |
| Ingest | `remember` → `define_entity`, `add_records`, `save_concept`, `save_passages` |
| Query | `search_text` (full text of passages, bodies and sources), `search_ontology` (Terms, record values), `get_term`, `check_answerable`, `run_sql` |
| Filing good answers back | `save_concept(append_body=...)`, `save_example_query` |
| Lint | `review_ontology` prompt; `check_readiness` counts empty concepts and records without a source |
| `index.md` | `list_terms` |
| `log.md` | `recent_changes` |
| Obsidian as the viewer | `export_wiki`: one page per Term (synonyms as aliases, relations as `[[links]]`, its passages), one per source (link to the original, its passages), plus `index.md` and `log.md`, next to the originals |

What the structure adds on top of markdown pages:

- **Numbers you can trust.** "How much on eating out in October?" is a SQL sum over typed records, checked by `validate_sql`, not arithmetic over prose.
- **Provenance per fact.** Each record carries its source, locator and quote, so an answer can say which receipt or page it came from.
- **No silent duplicates or broken data.** Keys and duplicate detection stop the same receipt from being stored twice, and invalid dates, amounts or columns reject the whole batch with reasons.
- **Typed links.** `is_a`, `part_of`, `causes` and the like instead of bare backlinks, so "what are the parts of X" has an answer, in SQL if you like.
- **Search and lookups in one query.** A full-text match, a walk over relations and a filter on records can share one SQL statement.
- **Review states.** Draft, certified and deprecated, as in Auto Ontology, so you know which definitions a person confirmed.

Limits to know about:

- Generated pages are a one-way view. Edits made to them in Obsidian are not read back; change knowledge through the assistant and run `export_wiki` again (it replaces only the pages it generated and never touches originals or pages you wrote).
- Search is lexical, not semantic: FTS5 matches words and prefixes, not meaning. Search with the words the sources use, add your own words as synonyms, and translate when the sources are in another language.
- The store indexes passages, not whole originals. What the model did not save as a passage or summary is in the vault but not in `search_text`.
- Nothing watches your files or the vault. When a document changes, ask the assistant to read it again. Earlier versions of its original stay in the vault until you delete them.
- It is a single-user store on your machine. Back it up by copying `~/.auto-ontology-mcp`.

Large existing tables (bank exports, workout logs) load faster with [sqlite-utils](https://sqlite-utils.datasette.io/) into their own database, connected read-only next to the store:

```bash
uvx sqlite-utils insert ~/kb/personal.db spending ~/Downloads/card-export.csv --csv
uvx --from git+https://github.com/gogoymh/auto-ontology-mcp auto-ontology-mcp add personal "sqlite:///$HOME/kb/personal.db"
```

### From a chat app, with OpenClaw or Hermes Agent

[OpenClaw](https://docs.openclaw.ai) and [Hermes Agent](https://hermes-agent.nousresearch.com) are personal agents you run on your own machine or server and talk to from messaging apps. With auto-ontology-mcp registered ([setup](#quick-start)), you can send a photo from your phone and ask about it weeks later. The tools are the same; Hermes prefixes them with the server name:

```text
You     [photo of a receipt] lunch today

Agent   → mcp_auto_ontology_search_ontology(query="receipt", datasource="knowledge")
        → mcp_auto_ontology_add_records(entity="Receipt", records=[{...}],
              source={"title": "chat photo, 2026-11-06", "kind": "image"})

You     eating out this month vs last month?

Agent   → mcp_auto_ontology_check_answerable(...) → mcp_auto_ontology_run_sql(...)
```

Reading the photo is up to the agent's model, so use one that accepts images. The server runs where the agent runs: `~/.auto-ontology-mcp` is that machine's home directory.

## Tools

| Stage | Tools |
| --- | --- |
| Status | `check_readiness` |
| Connect and scan | `add_datasource`, `scan_datasource` |
| Model | `get_modeling_worklist`, `save_table_terms`, `update_term`, `save_glossary` |
| Relationships | `list_relationships`, `set_relationship`, `find_join_path` |
| SQL attributes | `suggest_sql_attributes`, `save_sql_attribute` |
| Explore | `search_ontology`, `get_term`, `describe_table`, `lookup_values` |
| Answer | `check_answerable`, `get_query_context`, `validate_sql`, `run_sql`, `save_example_query` |
| Knowledge store | `define_entity`, `add_records`, `edit_records`, `save_concept`, `save_passages`, `search_text`, `get_source`, `list_terms`, `recent_changes`, `export_wiki` |
| Share | `export_ontology`, `import_ontology` |
| ChatGPT connectors | `search`, `fetch` |

Prompts: `build_ontology`, `answer_question`, `remember`, `review_ontology`. Resource: `ontology://guide`. The server also sends its workflow as MCP instructions at handshake, so the model knows to check answerability and validate before it runs anything.

Read tools carry `readOnlyHint`, and tools that can delete carry `destructiveHint`, so clients can skip or require confirmation accordingly.

A client that only asks questions of databases someone has already modeled can set `AUTO_ONTOLOGY_TOOLSETS=answer`: the server then exposes the 13 tools in the Status, Explore and Answer rows plus `find_join_path`, `search` and `fetch` and leaves the modeling and knowledge-store steps out of its handshake instructions. That cuts the tool definitions the client sends with every request from about 52,000 characters to about 12,000. Add `model`, `knowledge` or `share` to the list to expose those groups as well.

## How it works

```mermaid
flowchart LR
    U([You]) <--> M["Your MCP client's model<br/>Claude Code, Codex, OpenClaw,<br/>Hermes, Cursor, ...<br/>names Terms, reviews joins,<br/>extracts entities, writes SQL"]
    M <-- "MCP<br/>stdio or HTTP" --> S["auto-ontology-mcp<br/>scans, profiles, infers keys,<br/>validates, stores, runs read-only"]
    S <--> O[("ontology.db<br/>Terms, relationships,<br/>SQL attributes")]
    S <-- "validated writes" --> K[("knowledge.sqlite<br/>records, sources, passages,<br/>Term mirror, FTS5 indexes")]
    S -- "originals and pages" --> V[("Obsidian vault")]
    S -- "read-only" --> D[("Your databases")]
```

Each step of Auto Ontology maps to one side of that line:

| Step | NVIDIA Auto Ontology | auto-ontology-mcp |
| --- | --- | --- |
| Schema extraction and profiling | Ingestion service | Server: `scan_datasource` |
| Undeclared foreign keys | LLM picks candidate columns, vector search finds the target key, a SQL probe checks values | Server: name rules plus the same value probe; ambiguous cases go to the model |
| Terms and column mapping | LLM, one table at a time | The model, with the same rules, via `get_modeling_worklist` and `save_table_terms` |
| SQL attribute suggestions | Expressions mined from the warehouse's query history, judged by an LLM | Server mines the queries run through it and the verified examples (not the warehouse's own history), the model judges, the server test-runs before saving |
| Joins seen in query history | Recorded from the warehouse's query logs | Joins used by executed and verified queries that are not relationships yet, listed by `list_relationships` for review |
| Answerability | LLM entity extraction plus embedding search | The model extracts entities; the server matches them lexically and against stored values |
| Writing SQL | LLM agent | The model |
| Checking SQL | Database probes plus an LLM review | Server: database probes and ontology checks in `validate_sql` |
| Storage | Postgres and pgvector | Local SQLite files |
| Ingesting knowledge | Ingestion service reads databases | The model reads files, images and notes; the server validates and stores the records, concepts and sources it extracts |
| Schema changes | Scheduled re-ingest diffs the catalog; semantic compilation models only tables that have no Term yet | `scan_datasource` diffs the catalog; the worklist returns new tables and modeled tables that gained columns |

Search is lexical because there is no embedding model: token overlap, trigram similarity and phrase match for Terms and columns, and SQLite FTS5 with bm25 for the text of passages, bodies and sources. The server instructions ask the model to search with the schema's vocabulary, translate non-English phrases, and add the user's own words as synonyms with `update_term(add_synonyms=[...])`, after which they match directly.

Not carried over from Auto Ontology: the web UI and REST API, users, groups and RBAC, zones (access scoping), tags and rules, PII detection, JSONB path checks, embedding search and reranking, chart generation, KumoRFM predictions and Apache Ossie interchange (export and import use this project's own YAML format). Added here and not in Auto Ontology: write access over MCP, the personal knowledge store (Auto Ontology ingests databases, not documents), concept pages with typed relations, provenance on records, passages with full-text search, originals kept in Obsidian, the Obsidian export, and returning columns added to modeled tables to the model after a re-scan.

## Safety

The server writes in exactly one place: `~/.auto-ontology-mcp/knowledge.sqlite`, through the knowledge tools, which build parameterized statements from validated input. `run_sql` cannot write there either. Connected databases are only ever read, enforced in three layers:

1. **SQL guard.** One `SELECT`, `WITH` or set operation per call. DML, DDL, `PRAGMA`, `ATTACH`, `SELECT … INTO`, `FOR UPDATE` and functions that reach files, other servers or server state (`pg_read_file`, `dblink`, `LOAD_FILE`, `set_config`, `load_extension`, ...) are refused. Row sources are allow-listed against the scanned catalog, which also blocks `sqlite_master`, DuckDB's `FROM '/path/to/file'` and `OPENQUERY`. Oracle's one-row `DUAL` is the only extra source allowed. On Oracle the guard also refuses:
   - calls into `DBMS_*`/`UTL_*` packages and system owners (`SYS`, `CTXSYS`, `XDB`, ...), even when qualified (`sys.dbms_xmlgen.getxml(...)`) or written without parentheses (`utl_inaddr.get_host_name`);
   - table aliases and CTE names spelled like those packages or owners (`FROM orders utl_x`), because such an alias could hide a package call;
   - the URI types (`HTTPURITYPE`, ...) and `BFILENAME`;
   - sequence `NEXTVAL`/`CURRVAL`.
2. **Read-only connections.** SQLite opens with `mode=ro`, PostgreSQL with `default_transaction_read_only`, DuckDB with `read_only`, MySQL with a read-only session, and Oracle runs `SET TRANSACTION READ ONLY` at the start of every transaction. A PL/SQL function declared `PRAGMA AUTONOMOUS_TRANSACTION` can still write outside that transaction, so on Oracle a read-only account matters even more.
3. **Rollback.** Every transaction is rolled back.

Queries time out after 30 seconds and return at most 500 rows (100 by default).

Over HTTP, a token is required to reach the server from anywhere but localhost: without one, requests addressed to any other host are refused as DNS-rebinding protection, which also means a tunnel will not work until you set `--token`.

> [!WARNING]
> Datasource URLs, passwords included, are stored in plain text in `~/.auto-ontology-mcp/ontology.db`. Tool output masks them. Use a read-only database account, and on shared machines set `AUTO_ONTOLOGY_ALLOW_DATASOURCE_TOOLS=0` so datasources can only be added from the CLI.

## Configuration

<details>
<summary>Environment variables</summary>

| Variable | Default | Meaning |
| --- | --- | --- |
| `AUTO_ONTOLOGY_HOME` | `~/.auto-ontology-mcp` | Where the ontology store, the knowledge store (`knowledge.sqlite`) and the demo database live |
| `AUTO_ONTOLOGY_STORE` | `$AUTO_ONTOLOGY_HOME/ontology.db` | Ontology store file |
| `AUTO_ONTOLOGY_VAULT` | unset | Your Obsidian vault. Originals and generated pages go to its `auto-ontology/` folder, and `original` paths are relative to it. Unset: `$AUTO_ONTOLOGY_HOME/wiki/knowledge` is the vault |
| `AUTO_ONTOLOGY_MAX_ROWS` | `500` | Upper bound on rows `run_sql` returns |
| `AUTO_ONTOLOGY_QUERY_TIMEOUT_S` | `30` | Per-query timeout in seconds |
| `AUTO_ONTOLOGY_SCAN_TABLES` | unset | Default table filter for scans, comma-separated globs (`SALES_*,HR.EMP*,!*_LOG`). Unset: every table. See [Large schemas](#large-schemas) |
| `AUTO_ONTOLOGY_ORACLE_CLIENT_LIB` | unset | Oracle thick mode (for native network encryption): Instant Client directory, or `1` for the client on the library path. Unset: thin mode. Both need Oracle 12.1+ |
| `AUTO_ONTOLOGY_PROFILE_ROWS` | `1000` | Rows sampled per table while profiling |
| `AUTO_ONTOLOGY_ALLOW_DATASOURCE_TOOLS` | `1` | `0` hides `add_datasource` and `scan_datasource` from MCP |
| `AUTO_ONTOLOGY_TOOLSETS` | `all` | Tool groups to expose, comma-separated: `answer` (always on), `model`, `knowledge`, `share` |
| `AUTO_ONTOLOGY_MCP_TOKEN` | unset | Token required by the HTTP transport (same as `--token`) |
| `AUTO_ONTOLOGY_MCP_HOST` / `_PORT` | `127.0.0.1` / `8765` | HTTP bind address |
</details>

<details>
<summary>CLI</summary>

```text
auto-ontology-mcp                      stdio MCP server (what desktop clients launch)
auto-ontology-mcp serve --transport http --port 8765 --token ...
auto-ontology-mcp demo                 the retail database 'demo' and three demo documents in the knowledge store
auto-ontology-mcp demo --remove        remove the demo database registration and everything the demo documents added
auto-ontology-mcp add NAME URL         register and scan a datasource
auto-ontology-mcp scan NAME            re-scan; Terms and reviewed joins are kept, new tables and columns are reported
auto-ontology-mcp list                 readiness of every datasource
auto-ontology-mcp export NAME -o ontology.yaml
auto-ontology-mcp export-wiki -o ~/vault/ontology   Obsidian vault of the knowledge store
auto-ontology-mcp import NAME ontology.yaml
auto-ontology-mcp remove NAME
auto-ontology-mcp config               print client configuration snippets
```
</details>

## Development

Pull requests are welcome. [CONTRIBUTING.md](CONTRIBUTING.md) explains how to send one.

```bash
git clone https://github.com/gogoymh/auto-ontology-mcp && cd auto-ontology-mcp
uv sync
uv run pytest
```

The PostgreSQL tests run when `AO_TEST_PG_URL` points at a disposable database:

```bash
docker run -d --rm --name aomcp-pg -e POSTGRES_PASSWORD=pw -p 55432:5432 postgres:17
AO_TEST_PG_URL=postgresql+psycopg://postgres:pw@127.0.0.1:55432/postgres uv run --extra postgres pytest
```

The Oracle tests run when `AO_TEST_ORACLE_URL` points at a disposable user. The test drops and recreates the demo tables in that user's schema:

```bash
docker run -d --rm --name aomcp-ora -e ORACLE_PASSWORD=pw -e APP_USER=ao -e APP_USER_PASSWORD=pw -p 1521:1521 gvenzl/oracle-free
AO_TEST_ORACLE_URL="oracle+oracledb://ao:pw@127.0.0.1:1521/?service_name=FREEPDB1" uv run --extra oracle pytest tests/test_oracle.py
```

| Module | Responsibility |
| --- | --- |
| `catalog.py`, `profiling.py`, `inference.py` | Schema extraction, profiling, key and bridge-table inference |
| `knowledge.py` | The writable knowledge store: entities, records, concepts, relations, sources |
| `textindex.py` | Passages, the SQL mirror of Terms and relations, FTS5 indexes and `search_text` |
| `vault.py` | Originals in the Obsidian vault: linking, writing and copying them |
| `wiki.py` | Obsidian vault export |
| `demo.py`, `demo_knowledge.py` | The demo database and the demo documents |
| `service.py` | Everything behind the tools |
| `validation.py`, `sqlcheck.py`, `structure.py` | Read-only guard, column lineage, probe-based validation, checks on the parsed query |
| `search.py`, `graph.py` | Lexical search, join paths |
| `guidance.py` | Rules handed to the model, adapted from Auto Ontology's prompts |
| `server.py`, `http_auth.py`, `__main__.py` | MCP surface, HTTP token, CLI |

## Support the project

If Auto Ontology MCP saved you from a wrong number or replaced a pile of notes, a star on GitHub is the simplest way to help: it is how other people find the project. Bug reports and ideas are welcome in [Issues](https://github.com/gogoymh/auto-ontology-mcp/issues).

<p align="center">
  <a href="https://github.com/gogoymh/auto-ontology-mcp"><img src="docs/assets/star.gif" width="380" alt="Animation: a cursor clicks the Star button and it turns into Starred"></a>
</p>

## Acknowledgements

Built on the ideas and code of [NVIDIA Auto Ontology](https://github.com/NVIDIA/auto-ontology) (Apache-2.0): its modeling, relationship, SQL attribute, entity extraction and SQL rules, column profiling, expression mining and database probes. [NOTICE](NOTICE) lists what came from which file.

Uses [FastMCP](https://github.com/jlowin/fastmcp), [SQLAlchemy](https://www.sqlalchemy.org/) and [sqlglot](https://github.com/tobymao/sqlglot).

## Disclaimer

This is a personal open-source project. I built it on my own time and with my own resources, as a contribution to the open-source community.

- It is not affiliated with, sponsored by or endorsed by NVIDIA, or by any company or organization I am associated with. "Auto Ontology" refers to the NVIDIA project this work builds on.
- It is provided "as is", without warranty of any kind, under the [Apache License 2.0](LICENSE). Use it at your own risk.
- You decide what it touches. Review which databases and files you connect, and keep in mind that what the server returns is passed to the model behind your MCP client and handled under that provider's terms.

## License

[Apache License 2.0](LICENSE)
