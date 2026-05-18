#!/bin/csh -f
# Usage:
#   ./run_dir_scan.csh --workers 64
#
# config.list: list of python3 paths (one path per line).
# The script picks the first existing path in config.list and runs:
#   bs -os RHEL8 -M 10000 <python_path> disk_scan.py --workers <N>

set workers = 64

set script_dir = "$0:h"
if ("$script_dir" == "") set script_dir = "."
set script_dir = `cd "$script_dir" && pwd`
set config_file = "$script_dir/config.list"
set scan_script = "$script_dir/disk_scan.py"

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

if (! -f "$config_file") then
  echo "config.list not found: $config_file"
  exit 2
endif

if (! -f "$scan_script") then
  echo "disk_scan.py not found: $scan_script"
  exit 2
endif

set pybin = ""
foreach p (`awk 'NF && $1 !~ /^#/' "$config_file"`)
  if (-e "$p") then
    set pybin = "$p"
    break
  endif
end

if ("$pybin" == "") then
  echo "No python path found in config.list"
  exit 3
endif

echo "Running: bs -os RHEL8 -M 10000 $pybin $scan_script --workers $workers"
bs -os RHEL8 -M 10000 "$pybin" "$scan_script" --workers "$workers"
exit $status
