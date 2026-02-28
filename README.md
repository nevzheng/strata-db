# 🌥️ strata-db — a layered database you can take apart

StrataDB is an educational SQL database built in Rust, designed to make every layer of database engineering visible and swappable. It takes an embedded key-value store ([sled](https://github.com/spacejam/sled)) and builds every layer of a SQL database on top of it — key encoding, row encoding, schema catalog, indexing, query execution, and SQL parsing.

This is not a production database. It's a playground for exploring database engineering trade-offs.

## Architecture

```
SQL Parser  (sqlparser-rs)
     ↓
Query Executor
     ↓
Table API
     ↓
Row Encoding / Key Encoding
     ↓
Schema Catalog
     ↓
Storage Trait  ← pluggable
     ↓
sled  ← default backend
```

## Roadmap

- [ ] Storage trait + sled backend
- [ ] Key encoder
- [ ] Row encoder
- [ ] Schema catalog
- [ ] Table API (create, insert, get, scan, delete)
- [ ] Secondary indexes
- [ ] SQL parsing + REPL
- [ ] Query executor (scan, filter, project, joins, aggregations)
- [ ] DSM (column-store) table layout
- [ ] Parquet export

## Writing

I'm blogging about the design decisions and trade-offs along the way at [n8z.dev](https://n8z.dev).
