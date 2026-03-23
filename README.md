# Fünke 
![Tobias Fünke, who just "blue" himself](./docs/bluemyself.jpg)

A lightweight, TUI bluetooth manager, written in Rust. Manage connections, pair devices, and more.

## Features
![TUI preview](./docs/preview.png)
- List available Bluetooth devices
- Connect to devices
- Pair and unpair devices
- View connection status
- Switch audio and input profiles

## Installation
*TODO*

## TODO / Roadmap

- Fünke CLI, for people who prefer command line tools

- Automate new releases with GitHub Actions

- Allow launch when adapter is off, and adapter management. Following error right now: 
```
Error: Could not find a Bluetooth adapter: org.freedesktop.DBus.Error.UnknownObject: Method "Get" with signature "ss" on interface "org.freedesktop.DBus.Properties" doesn't exist

Make sure BlueZ is installed and the bluetooth service is running.
```
## AI Usage Disclosure 
This project was developed with the assistance of AI tools, specifically Claude Code managed through a Ralph loop. 
