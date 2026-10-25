#!/bin/bash
# runs under lock.sh laptop: ours A, linux 1, ours B, linux 2, ours C
D=$(cd $(dirname $0)/.. && pwd); rm -rf $D/results-ours; mkdir -p $D/results-ours $D/campaign
for k in 1 2; do
  bash $D/scripts/ours.sh o$k
  rm -rf $D/results; mkdir $D/results; bash $D/scripts/seq.sh; echo "seq$k rc=$?" >> $D/results-ours/campaign.txt
  cp -R $D/results $D/campaign/linux$k
done
bash $D/scripts/ours.sh o3
echo DONE >> $D/results-ours/campaign.txt
