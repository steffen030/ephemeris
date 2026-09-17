# Motivation
Ephememris is an astronomers table for tracking positions over time. This project is to track your work and life over time. It's supposed to be a highly customizable app that is focused on eink devices with pens, like pinenote. but potentially also a kobo or jailbroken kindle. The pen is the interactive enabler and allows to trigger actions unlike classical pen applications that focus on writing alone

# Features
- Is able to manage different profiles (like work, home, etc.) while managing to create a holistic overview
- The UI is modern an minimalistic on the daily use while configs allow high customization
- Snappy navigation on eink devices
- Can connect various calendar provider (caldav/ https feed)
- Can connect with various task providers
- Can interact with an obsidian repository, extract tasks, find and read notes, export notes to obsidian repo - still not sure on what is the best path here. 
- Can synchronize files with webdav (later google drive)

# start page
- overview of day/week/month
- overview of tasks ahead - clear recommendation using prioritisation
- always accessible action ring that allows to quickly capture a new note, take audio recording (for devices with mic like pinenote), add a calendar entry, a task

# note taking
- writting area needs to be fullscreen, almost all of the screen should be available for note taking
- on pinenote with gnome - wipping down reveals status bar until tipping back on the app
- long press on upper area with hand will bring wider menu of ephemeris back until tapping back on the note taking page
- top burger menue allows to configure generic note taking configs (like background for current note page - lines, squares,...)
- tapping with pen while emr button pressed will trigger note taking action item on that point (like adding calendar entry that gets materialed in that note on the pointed place and also to the calendar or adding a task materialized in task list and also on that note) or changing pen (going eraser for example)
- notes should be exported as pdf, ideally with transcribed hand writing. later edits should update the pdf
- need to pay attention that handbalm will not interfere with writing
- swipping left and right will turn pages (and create following page if it's not existing yet)


# Architecture & Design
- App operates on eink device on full screen
- use libraries wherever possible. we don't want to reinvent the wheel
- probably implement in rust