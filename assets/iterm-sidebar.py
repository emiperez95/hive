#!/usr/bin/env python3
"""hive sidebar — register the iTerm2 Toolbelt panel. Installed by `hive setup`.

Shows hive's conversation sidebar in iTerm's Toolbelt (View > Toolbelt,
Cmd+Shift+B), served by the hive web dashboard.

This is NOT a daemon. iTerm stores the registration in its own preferences, so
the panel keeps working after this script exits — it only needs to run again
when the URL changes, which is why it lives in AutoLaunch. The URL carries
hive's version so that upgrading forces the webview to reload rather than
serving the previous release's page from cache.

Requires "Enable Python API" (iTerm2 > Settings > General > Magic).
"""

import iterm2

URL = "http://127.0.0.1:__PORT__/?sidebar=1&v=__VERSION__"
IDENTIFIER = "com.hive.sidebar"
DISPLAY_NAME = "hive"


async def main(connection):
    await iterm2.tool.async_register_web_view_tool(
        connection,
        display_name=DISPLAY_NAME,
        identifier=IDENTIFIER,
        # False: re-registering on every iTerm launch should refresh the URL,
        # not pop the Toolbelt open at someone who closed it.
        reveal_if_already_registered=False,
        url=URL,
    )


iterm2.run_until_complete(main)
