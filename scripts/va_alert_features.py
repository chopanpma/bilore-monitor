import pandas as pd, numpy as np, sys, warnings; warnings.filterwarnings('ignore')
d=pd.read_pickle(sys.argv[1]); sub=sys.argv[2] if len(sys.argv)>2 else 'all'
if sub=='rth': d=d[d.kind=='rth']
sg=np.where(d.fade=='long',1,-1)
d['move5_f']=sg*d.move5; d['move15_f']=sg*d.move15; d['imb5_f']=sg*d.imb5; d['imb15_f']=sg*d.imb15; d['delta_f']=sg*d.delta
d['large_f']=sg*(d.large_buy-d.large_sell)
dates=sorted(d.date.unique()); mid=dates[len(dates)//2]
F={
 'structure agrees':d.structure==1,'structure against':d.structure==-1,
 'order flow agrees':d.order_flow==1,'order flow against':d.order_flow==-1,
 'OTF agrees':d.otf==1,'OTF against':d.otf==-1,
 '2+/3 aligned':d.agreeing>=2,
 'shape D':d['shape']=='D','shape P':d['shape']=='P','shape b':d['shape']=='B',
 'P/b and fade toward its bulge':((d['shape']=='P')&(d.fade=='long'))|((d['shape']=='B')&(d.fade=='short')),
 'upper (fade short)':d.side=='upper',
 'early (period<=3)':d.period<=3,
 'quiet: 30m range<10':d.range30<10,
 'burst: |5m move|>=4':d.move5.abs()>=4,
 'burst into the extreme (5m move >=4 against fade)':d.move5_f<=-4,
 'already turning (5m move >=1 with fade)':d.move5_f>=1,
 'still pushing out (5m move <=-1)':d.move5_f<=-1,
 '5m flow with fade (imb>=0.10)':d.imb5_f>=0.10,
 '5m flow against fade (imb<=-0.10)':d.imb5_f<=-0.10,
 'absorption at extreme (flow out, price stuck)':(d.imb5_f<=-0.10)&(d.move5_f>=-1),
 '15m flow with fade':d.imb15_f>=0.10,
 'beyond edge >=1pt':d.beyond_edge>=1,
 'POC >=8pt away':d.to_poc>=8,
 'VA width >=15pt':d.va_width>=15,
 'large prints net with fade':d.large_f>0,
 'prior RTH: inside prior value':d.prior_loc=='inside',
 'prior RTH POC lean agrees':d.prior_lean_agrees==1,
}
M=sys.argv[3] if len(sys.argv)>3 else 'pnl_poc_6'; W=M.replace('pnl','win')
base=d[M].mean()
rng=np.random.default_rng(0)
def boot(mask):
    # session-cluster bootstrap of the difference in mean pnl (in - out)
    g=d.assign(m=mask.values).groupby('date'); keys=list(g.groups)
    parts={k:v for k,v in g}
    diffs=[]
    for _ in range(400):
        s=pd.concat([parts[k] for k in rng.choice(keys,len(keys))])
        a,b=s[s.m][M],s[~s.m][M]
        if len(a)>3 and len(b)>3: diffs.append(a.mean()-b.mean())
    return np.percentile(diffs,[5,95])
rows=[]
for name,m in F.items():
    m=m.fillna(False).astype(bool)
    a,b=d[m],d[~m]
    if len(a)<15 or len(b)<15: continue
    h1=a[a.date<mid][M].mean()-b[b.date<mid][M].mean(); h2=a[a.date>=mid][M].mean()-b[b.date>=mid][M].mean()
    lo,hi=boot(m)
    rows.append((name,len(a),a.date.nunique(),a[W].mean(),b[W].mean(),a[M].mean(),b[M].mean(),a[M].mean()-b[M].mean(),h1,h2,lo,hi))
r=pd.DataFrame(rows,columns=['feature','n','sess','win_in','win_out','pnl_in','pnl_out','diff','H1diff','H2diff','ci5','ci95']).sort_values('diff',ascending=False)
print(f'{sub} n={len(d)} metric={M} base pnl {base:.2f} win {d[W].mean():.3f}  (split {mid})')
print(r.round(2).to_string(index=False))
