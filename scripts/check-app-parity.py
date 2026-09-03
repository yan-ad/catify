import argparse, json, pathlib, sys
p=argparse.ArgumentParser(); p.add_argument('inventory',type=pathlib.Path); p.add_argument('--manifest',type=pathlib.Path,default=pathlib.Path('inventory/app-command-parity.json')); a=p.parse_args()
inv={x['name'] for x in json.loads(a.inventory.read_text())['commands'] if x['name'].startswith('app ')}
manifest=json.loads(a.manifest.read_text())['commands']; missing=sorted(inv-set(manifest)); extra=sorted(set(manifest)-inv); invalid=[n for n,v in manifest.items() if v.get('status') not in {'native','partial','adapter','blocked-live'} or not isinstance(v.get('owner'),int) or not isinstance(v.get('json'),bool) or not isinstance(v.get('non_interactive'),bool)]
if missing or extra or invalid:
 print(json.dumps({'missing':missing,'extra':extra,'invalid':invalid},indent=2),file=sys.stderr); raise SystemExit(1)
from collections import Counter
print(json.dumps({'commands':len(manifest),'status':Counter(v['status'] for v in manifest.values())},default=dict))
