import json, glob, os
recdir = os.path.expanduser("~/.pancetta/recordings")
mans = glob.glob("research/corpus/curated/ft8/**/*.manifest.json", recursive=True)
union_present = set(); per = {}
for m in sorted(mans):
    try: data = json.load(open(m))
    except Exception as e: print("SKIP", m, e); continue
    entries = data.get("entries", []) if isinstance(data, dict) else (data if isinstance(data, list) else [])
    present=missing=0; names=set()
    for e in entries:
        wp = e.get("wav_path") if isinstance(e, dict) else None
        if not wp: continue
        base = os.path.basename(os.path.expanduser(wp))
        if os.path.exists(os.path.join(recdir, base)):
            names.add(base); present+=1
        else:
            missing+=1
    union_present |= names
    per[os.path.basename(m)] = (len(entries), present, missing)
print(f"{'manifest':40} {'entries':>7} {'present':>7} {'missing':>7}")
for m,(t,p,mi) in per.items(): print(f"{m:40} {t:>7} {p:>7} {mi:>7}")
nonft8 = sorted(n for n in union_present if not n.startswith("ft8_"))
print(f"\nUNION present-on-disk: {len(union_present)} files")
print(f"  of which non-ft8_ (diagnostic): {len(nonft8)} -> {nonft8}")
ft8tot = len(glob.glob(os.path.join(recdir,'ft8_*.wav')))
print(f"recordings ft8_*.wav total: {ft8tot}  | union is {len(union_present)} of them")
open("/tmp/corpus_union.txt","w").write("\n".join(sorted(union_present)))
print("union written to /tmp/corpus_union.txt")
