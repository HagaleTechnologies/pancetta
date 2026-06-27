import json, glob, os
cor = os.path.expanduser("~/.pancetta/corpus")
COR_ABS = "/Users/thagale/.pancetta/corpus"
mans = glob.glob("research/corpus/curated/ft8/**/*.manifest.json", recursive=True)
print(f"{'manifest':40} {'before':>6} {'after':>6} {'pruned':>6}")
for m in sorted(mans):
    data = json.load(open(m))
    is_list = isinstance(data, list)
    entries = data if is_list else data.get("entries", [])
    before = len(entries)
    kept = []
    for e in entries:
        if not isinstance(e, dict): continue
        wp = e.get("wav_path")
        if not wp: continue
        base = os.path.basename(os.path.expanduser(wp))
        if os.path.exists(os.path.join(cor, base)):
            e["wav_path"] = f"{COR_ABS}/{base}"
            kept.append(e)
        # else: dangling -> pruned
    after = len(kept)
    if is_list:
        out = kept
    else:
        data["entries"] = kept
        # update common count fields if present
        for k in ("entry_count","count","n_entries","num_entries"):
            if k in data: data[k] = after
        out = data
    json.dump(out, open(m, "w"), indent=2)
    open(m, "a").write("\n")
    print(f"{os.path.basename(m):40} {before:>6} {after:>6} {before-after:>6}")
