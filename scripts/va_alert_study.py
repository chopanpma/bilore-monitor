"""Value-area alert study (MON-011, 2026-10-26): score the rows written by
`cargo run --release --bin backtest_va_alert` and test each cockpit variable.

Usage (bilore-ml venv has pandas):
  ../bilore-ml/.venv/bin/python scripts/va_alert_study.py va_alert_study.csv [all|rth] [pnl_poc_6|pnl_6_6|pnl_4_4|pnl_poc_8]
Step 1 adds outcomes (pickle next to the CSV), step 2 prints the feature table
(session-cluster bootstrap 90% CI, H1/H2 split by session date).
"""
import pandas as pd, numpy as np, os, subprocess, sys
d=pd.read_csv(__import__('sys').argv[1]); d['t']=pd.to_datetime(d.ts_ct)
COST=0.5
def first(row,k,kind):
    v=row[f'{kind}{k}']; return np.inf if pd.isna(v) else v
def res(row,T,S):
    tf,ta=first(row,T,'fav'),first(row,S,'adv')
    if tf<ta: return T-COST,1
    if ta<tf: return -S-COST,0
    return row.close_move-COST,np.nan  # neither
def resp(row,S):
    tp=np.inf if pd.isna(row.poc_s) else row.poc_s; ta=first(row,S,'adv')
    if tp<ta: return row.to_poc-COST,1
    if ta<tp: return -S-COST,0
    return row.close_move-COST,np.nan
for T,S in [(4,4),(6,6),(4,6),(8,6),(6,4)]:
    r=d.apply(lambda x: res(x,T,S),axis=1); d[f'pnl_{T}_{S}']=[a for a,b in r]; d[f'win_{T}_{S}']=[b for a,b in r]
for S in [4,6,8]:
    r=d.apply(lambda x: resp(x,S),axis=1); d[f'pnl_poc_{S}']=[a for a,b in r]; d[f'win_poc_{S}']=[b for a,b in r]
d.to_pickle(__import__("sys").argv[1]+".pkl")
def summ(g,cols=('win_6_6','pnl_6_6','win_poc_6','pnl_poc_6')):
    return pd.Series({'n':len(g),'sess':g.date.nunique(),**{c:g[c].mean() for c in cols}})
print('ALL candidates'); print(summ(d).round(2).to_string())
print('\nby sent'); print(d.groupby('sent').apply(summ).round(2).to_string())
print('\nby agreeing'); print(d.groupby('agreeing').apply(summ).round(2).to_string())
if __name__ == "__main__":
    here = os.path.dirname(os.path.abspath(__file__))
    subprocess.run([sys.executable, os.path.join(here, "va_alert_features.py"), sys.argv[1] + ".pkl", *sys.argv[2:]], check=True)
