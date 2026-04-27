# Aptmatic Requirements

Aptmatic is a CLI / TUI tool that can allow a user to manage apt installs across an arbitrary number of debian and ubuntu hosts. It is written in rust, to guarantee there are no bugs.

## Features

- The list of hosts is defined in a toml file. Hosts can be grouped into named groups
- Aptmatic interacts with each host over ssh
- The TUI should be beautiful and easy to use with keyboard shortcuts.
- The inteface should allow a user to see, for any given host:
  - The currently running kernel version
  - Whether a new kernel is pending activation on the next reboot
  - The number of apt packages that can be updated (and what those packages are)
  - Whether there are any packages that have been uninstalled but not purged ('rc' status) and the ability to purge those
  - Whether there are any updates that have been held back (and why)
- The interface should allow the user to trigger an "apt-get update", or an "apt-get upgrade" for one or a whole group of hosts
- The current ssh session with the host and text progress should be able to be viewed for any host that's in the process of running an apt task.
