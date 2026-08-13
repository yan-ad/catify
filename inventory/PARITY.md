# Shopify CLI parity inventory

Generated from Shopify CLI `4.6.0` at commit [`87a3ae19c8dd`](https://github.com/Shopify/cli/commit/87a3ae19c8ddc6bdb379d9d69068ad986995aa59).

Classifications: `native`, `adapter-backed`, `deferred`, or `unsupported`. This table is generated; edit classifications in the generator policy until implementation metadata is introduced.

| Command | Aliases | Flags | Env | Config | Executables | APIs | Status |
|---|---|---:|---:|---:|---|---|---|
| `app app-logs sources` | — | 0 | 12 | 0 | — | business-platform-graphql | `deferred` |
| `app build` | — | 1 | 13 | 0 | — | business-platform-graphql | `adapter-backed` |
| `app bulk cancel` | — | 2 | 13 | 0 | — | — | `deferred` |
| `app bulk execute` | — | 0 | 12 | 0 | — | — | `deferred` |
| `app bulk status` | — | 2 | 13 | 0 | — | — | `deferred` |
| `app config link` | — | 3 | 15 | 2 | — | business-platform-graphql | `deferred` |
| `app config pull` | — | 0 | 12 | 0 | — | business-platform-graphql | `deferred` |
| `app config use` | — | 0 | 12 | 3 | — | business-platform-graphql | `deferred` |
| `app config validate` | — | 0 | 12 | 8 | — | business-platform-graphql | `deferred` |
| `app demo watcher` | — | 0 | 12 | 0 | — | business-platform-graphql | `deferred` |
| `app deploy` | — | 7 | 18 | 0 | — | business-platform-graphql | `deferred` |
| `app dev clean` | — | 1 | 12 | 0 | — | business-platform-graphql | `deferred` |
| `app dev` | — | 15 | 26 | 2 | — | business-platform-graphql | `adapter-backed` |
| `app env pull` | — | 1 | 13 | 2 | — | business-platform-graphql | `deferred` |
| `app env show` | — | 0 | 12 | 0 | — | business-platform-graphql | `deferred` |
| `app execute` | — | 0 | 12 | 0 | — | — | `deferred` |
| `app function build` | — | 0 | 12 | 0 | — | business-platform-graphql | `deferred` |
| `app function info` | — | 0 | 12 | 0 | — | business-platform-graphql | `deferred` |
| `app function replay` | — | 1 | 13 | 0 | — | business-platform-graphql | `deferred` |
| `app function run` | — | 3 | 15 | 0 | — | business-platform-graphql | `deferred` |
| `app function schema` | — | 1 | 13 | 0 | — | business-platform-graphql | `deferred` |
| `app function typegen` | — | 0 | 12 | 3 | — | business-platform-graphql | `deferred` |
| `app generate extension` | — | 2 | 16 | 2 | — | business-platform-graphql | `deferred` |
| `app graphiql` | — | 4 | 13 | 0 | — | — | `deferred` |
| `app import-custom-data-definitions` | — | 2 | 13 | 2 | — | admin-graphql, business-platform-graphql, storefront | `deferred` |
| `app import-extensions` | — | 1 | 12 | 1 | — | business-platform-graphql | `deferred` |
| `app info` | — | 1 | 14 | 0 | — | business-platform-graphql | `deferred` |
| `app init` | — | 7 | 8 | 3 | — | business-platform-graphql, partners-graphql | `adapter-backed` |
| `app logs` | — | 3 | 14 | 2 | — | business-platform-graphql | `deferred` |
| `app release` | — | 3 | 14 | 0 | — | business-platform-graphql | `deferred` |
| `app versions list` | — | 0 | 12 | 0 | — | business-platform-graphql | `deferred` |
| `app webhook trigger` | — | 3 | 18 | 0 | — | business-platform-graphql | `deferred` |
| `organization list` | — | 0 | 0 | 0 | — | — | `deferred` |
| `auth login` | — | 0 | 1 | 0 | — | — | `deferred` |
| `auth logout` | — | 0 | 0 | 0 | — | — | `deferred` |
| `cache clear` | — | 0 | 0 | 0 | — | — | `deferred` |
| `config autoupgrade off` | — | 0 | 0 | 0 | — | — | `deferred` |
| `config autoupgrade on` | — | 0 | 0 | 0 | — | — | `deferred` |
| `config autoupgrade status` | — | 0 | 0 | 0 | — | — | `deferred` |
| `debug command-flags` | — | 1 | 1 | 0 | — | — | `deferred` |
| `doc fetch` | — | 2 | 2 | 0 | — | — | `deferred` |
| `doc search` | — | 3 | 3 | 0 | — | — | `deferred` |
| `docs generate` | — | 0 | 0 | 0 | — | — | `deferred` |
| `doctor-release doctor-release` | — | 0 | 0 | 0 | — | — | `deferred` |
| `help` | — | 1 | 1 | 0 | — | — | `deferred` |
| `kitchen-sink async` | — | 0 | 0 | 0 | — | — | `deferred` |
| `kitchen-sink prompts` | — | 0 | 0 | 0 | — | — | `deferred` |
| `kitchen-sink static` | — | 0 | 0 | 1 | — | partners-graphql | `deferred` |
| `notifications generate` | — | 0 | 0 | 1 | — | — | `deferred` |
| `notifications list` | — | 1 | 1 | 1 | — | — | `deferred` |
| `search` | — | 0 | 0 | 0 | — | — | `deferred` |
| `upgrade` | — | 0 | 0 | 0 | — | — | `deferred` |
| `version` | — | 0 | 0 | 0 | — | — | `deferred` |
| `store auth list` | — | 0 | 0 | 0 | — | — | `deferred` |
| `store auth` | — | 1 | 13 | 0 | — | — | `deferred` |
| `store bulk cancel` | — | 1 | 12 | 0 | — | — | `deferred` |
| `store bulk execute` | — | 0 | 12 | 0 | — | — | `deferred` |
| `store bulk status` | — | 1 | 12 | 0 | — | — | `deferred` |
| `store create dev` | — | 3 | 16 | 0 | — | business-platform-graphql | `deferred` |
| `store create preview` | — | 2 | 13 | 0 | — | admin-graphql | `deferred` |
| `store delete` | — | 1 | 13 | 0 | — | business-platform-graphql | `deferred` |
| `store execute` | — | 7 | 12 | 0 | — | — | `deferred` |
| `store graphiql` | — | 4 | 13 | 0 | — | — | `deferred` |
| `store info` | — | 0 | 12 | 0 | — | admin-graphql, business-platform-graphql | `deferred` |
| `store list` | — | 1 | 12 | 0 | — | business-platform-graphql | `deferred` |
| `store open` | — | 0 | 12 | 0 | — | — | `deferred` |
| `store stripe-auth` | — | 2 | 14 | 0 | — | — | `deferred` |
| `theme check` | — | 8 | 15 | 3 | — | — | `adapter-backed` |
| `theme console` | — | 2 | 8 | 1 | — | storefront | `deferred` |
| `theme delete` | — | 3 | 10 | 1 | — | — | `deferred` |
| `theme dev` | — | 18 | 23 | 2 | — | storefront | `adapter-backed` |
| `theme duplicate` | — | 2 | 9 | 1 | — | — | `deferred` |
| `theme info` | — | 2 | 9 | 1 | — | — | `deferred` |
| `theme init` | — | 2 | 8 | 1 | — | — | `deferred` |
| `theme language-server` | — | 0 | 0 | 1 | — | — | `adapter-backed` |
| `theme list` | — | 3 | 9 | 1 | — | — | `deferred` |
| `theme metafields pull` | — | 1 | 8 | 2 | — | — | `deferred` |
| `theme open` | — | 4 | 10 | 1 | — | — | `deferred` |
| `theme package` | — | 0 | 6 | 3 | — | — | `deferred` |
| `theme preview` | — | 5 | 11 | 1 | — | storefront | `deferred` |
| `theme profile` | — | 3 | 10 | 1 | — | storefront | `deferred` |
| `theme publish` | — | 0 | 8 | 1 | — | — | `deferred` |
| `theme pull` | — | 5 | 11 | 1 | — | — | `deferred` |
| `theme push` | — | 11 | 17 | 1 | — | — | `deferred` |
| `theme rename` | — | 3 | 10 | 1 | — | — | `deferred` |
| `theme share` | — | 2 | 8 | 1 | — | — | `deferred` |

## Scanner limitations

- Static conservative scan; dynamically composed flags and transitive runtime dependencies may be absent.
- Aliases are recorded when declared in command metadata; current source scan found none.
- API and executable fields are evidence markers, not a complete call graph.
