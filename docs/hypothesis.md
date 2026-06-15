> [Back to README](../README.md) | [All Specs](./spec/index.md)

# Django, Explicitness, and the Agentic Shift

This note explains why Djogi borrows some of Django's goals while rejecting its full-stack shape and much of its implementation style. Django is a reference point here, not a template.

## 1. What Django Actually Got Right

MVT (Model-View-Template) is the visible structure, but Django's real value came from something deeper: it reduced repeated work around the data layer and the operational plumbing around it.

- **DRY (Don't Repeat Yourself):** Define your data schema exactly once in `models.py`. Django then automatically derives the database migrations, the Python ORM API, and the Admin UI.
- **Loose Coupling:** Keep model logic, HTTP handling, and rendering from collapsing into one pile of code.
- **Security by Default:** Treat security as framework work rather than optional app glue.
- **Batteries Included:** Ship admin, auth, sessions, and ORM workflows together so ordinary applications can move quickly.

Those are still good goals. Djogi agrees with them where they apply to the data layer and to reusable tooling derived from it. Djogi is not trying to become a Rust version of Django's full application shell.

## 2. Where Django's Shape Stops Helping

What Djogi does not want to inherit is Django's tendency to hide important behavior behind broad framework layers.

In practice, a Django request passes through several subsystems that are productive once learned, but expensive to hold in your head:

| Component | The "Simplified" View | The Technical Reality |
|---|---|---|
| URL Router | Matches a URL to a file. | A routing and dispatch layer that acts as the entry point for controller logic. |
| Middleware | Not mentioned in MVT. | A cross-cutting request/response layer that can modify behavior before the view runs. |
| Managers/ORM | "The Database." | A sophisticated abstraction layer that translates Pythonic code into optimized SQL. |
| Forms/Serializers | "Part of the View." | A separate validation and coercion layer that shapes what may reach the model. |

None of that is inherently bad. The problem is that modern tooling changed what is expensive.

## 3. What Changed In The Agentic Era

The value of a "batteries included" framework changed once code generation and fast ecosystem integration became cheap.

| Feature | The Django Era (2005–2020) | The Agentic Era (2024+) |
|---|---|---|
| Main Hurdle | Integration: Connecting libraries was hard and slow. | Context: Managing complex, hidden abstractions is the new bottleneck. |
| Developer Speed | Provided by pre-written, high-level functions. | Provided by LLM generation and instant boilerplate creation. |
| Code Style | Magic & Abstraction: Hiding logic to save typing. | Explicit & Flat: AI performs better when logic is visible and type-safe. |
| Safety | Framework conventions (The "Django Way"). | Type systems (Rust/C#), AI-assisted security analysis, automated vulnerability detection, and model-driven fuzzing — the safety story now extends well beyond convention compliance. |
| Framework Role | A rigid foundation you build inside of. | A narrow data-layer runtime that owns the model derivation chain (ORM, migrations, admin, audit) while delegating routing, request lifecycle, and rendering to the ecosystem. |

## 4. What Djogi Takes From That

Django was liberating because it automated the expensive parts of application development. In an agentic workflow, a lot of former boilerplate is cheap. What remains expensive is the data layer: schema consistency, migration safety, audit trails, lock-aware write flows, and type-safe query construction over Postgres-native features.

That is the space Djogi wants to own.

Not the whole application shell.

Djogi keeps the part of Django that still matters:

- define the model once
- derive the surrounding data machinery from it
- make correctness and safety framework concerns where they are reusable

Djogi rejects the parts that now cost more than they save:

- full-stack ownership of routing and rendering
- ownership of the request lifecycle
- framework-owned app structure
- session or auth orchestration as core identity
- broad hidden behavior
- abstraction layers that make query count, lock semantics, or SQL shape harder to see

The result is not "Rust Django." It is a Postgres-native Rust data layer with strong model ergonomics, generated tooling, and explicit boundaries.

Djogi may ship optional admin or shell surfaces, but those are adapters built on top of the model/query runtime. They do not change the core claim: Djogi owns the data layer and its derivation chain, not the whole web application.
