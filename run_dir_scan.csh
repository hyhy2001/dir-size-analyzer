#!/bin/csh -f
# Usage:
#   ./run_dir_scan.csh --workers 64
#
# config.list: danh sách path python3 (mỗi dòng 1 path)
# Script lấy path đầu tiên TỒN TẠI trong config.list và chạy:
#   bs -os RHEL8 -M 10000 <python_path> disk_scan.py --workers <N>

set workers = 64

set i = 1
while ($i <= $#argv)
  set arg = "$argv[$i]"

  if ("$arg" == "--workers" || "$arg" == "--worker") then
    @ i++
    if ($i > $#argv) then
      echo "Missing value for $arg"
      exit 1
    endif
    set workers = "$argv[$i]"
  else if ("$arg" =~ --workers=*) then
    set workers = `echo "$arg" | sed 's/^--workers=//'`
  else if ("$arg" =~ --worker=*) then
    set workers = `echo "$arg" | sed 's/^--worker=//'`
  else
    echo "Unknown option: $arg"
    echo "Usage: $0 --workers 64"
    exit 1
  endif

  @ i++
end

if (! -f "config.list") then
  echo "config.list not found"
  exit 2
endif

set pybin = ""
foreach p (`awk 'NF && $1 !~ /^#/' config.list`)
  if (-e "$p") then
    set pybin = "$p"
    break
  endif
end

if ("$pybin" == "") then
  echo "No python path found in config.list"
  exit 3
endif

echo "Running: bs -os RHEL8 -M 10000 $pybin disk_scan.py --workers $workers"
bs -os RHEL8 -M 10000 "$pybin" disk_scan.py --workers "$workers"
exit $status
