# run1.sh <name> <build> <opts: env assignments or - ; "lazytime"/"nolazytime" remounts> -- tt-sustained args
N=$1; B=$2; E=$3; shift 4
O=$HOME/r/$N; [ -f $O/DONE ] && exit 0
rm -rf $O /data/tt; mkdir -p $O; sync; sleep 3
L=nolazytime; ENVS=""
for e in $E; do case $e in lazytime|nolazytime) L=$e;; -) ;; *) ENVS="$ENVS $e";; esac; done
sudo mount -o remount,noatime,$L /data; findmnt -no OPTIONS /data > $O/mount
timeout 1500 env $ENVS ~/b/$B/tt-sustained run --dir /data/tt --out $O/out --json "$@" > $O/stdout 2> $O/stderr; echo "exit $?" >> $O/stderr
rm -rf /data/tt; echo "$B $E $*" > $O/cmd; touch $O/DONE
