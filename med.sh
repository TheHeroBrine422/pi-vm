#!/bin/bash
# Median helper for boot timing lines. Usage: med.sh <logfile>
# Extracts "total X.Xs" from every 'boot timing' line and prints the median.
f=$1
grep "boot timing" "$f" 2>/dev/null | sed -E 's/.*total ([0-9.]+)s.*/\1/' | sort -n > /tmp/med-vals.$$
n=$(wc -l < /tmp/med-vals.$$)
[ "$n" -eq 0 ] && { echo "no values in $f"; exit 1; }
if [ $((n % 2)) -eq 1 ]; then sed -n "$(( (n+1)/2 ))p" /tmp/med-vals.$$; else awk -v n="$n" 'NR==n/2{a=$1} NR==n/2+1{printf "%.1f\n", (a+$1)/2}' /tmp/med-vals.$$; fi
rm -f /tmp/med-vals.$$
