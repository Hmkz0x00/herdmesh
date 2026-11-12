The brief builder takes a list of events and a `max_tokens` budget. It sorts by relevance (`open` > `blocked` > `done`, newer first), then appends items until the next one would overflow:

```rust
for item in ranked {
    let cost = estimate_tokens(&item.line);
    if used + cost > max_tokens { break; }
    used += cost;
    out.push_str(&item.line);
}
```

Example output for room `herdmesh-dev` at budget 80:

    goal: M1 proto+budgets | 3 open, 2 done
    - M1-2 budget estimator (hm-impl2) open
    - M1-3 room card (hm-impl1) open
    ref: adr:001, git:6fb926c
