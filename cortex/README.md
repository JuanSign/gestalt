# Cortex 
>"The wrinkles your network infrastructure was missing."

__Cortex__ is a <ins>high-performance</ins> multiplex communications protocol written in __Rust__. It serves as the critical entry point for the __Gestalt__ Project.

Let's consult the dictionary (_or a high school biology textbook_):  
__Cortex__ (_noun_): The outer layer of neural tissue of the cerebrum of the brain. It plays a key role in memory, attention, perception, awareness, thought, language, and consciousness. Basically, it's the part of the brain that deals with the outside world so the inner squishy parts don't have to.

This module is exactly that, but for packets.

Cortex acts as the "outer layer" of the Gestalt architecture. It sits boldly on the edge of the Public Internet, intercepting the chaotic screaming of web traffic, making sense of it (multiplexing), and calmly routing it to the internal organs of the Gestalt system.

If Gestalt is the soul, Cortex is the face that gets punched by the internet so the soul doesn't have to.

## Features
- __Blazingly Fast Multiplexing__  
We take a single connection and cram as many virtual streams into it as physics allows. It’s like a clown car, but for data.
- __Written in Rust__  
Because we like our memory safe and our borrow checkers strict. No null pointer exceptions here, just pure, unadulterated oxidation.
- __The Bouncer__  
It intercepts requests before they hit the sensitive internal services. _Sorry request, you're not on the list._
- __Gestalt Native__  
Designed specifically to support the Gestalt ecosystem. They go together like neurons and... other neurons.

## Usage
Look, we get it. Writing reliable network protocols is hard. Your current file-sharing method probably drops packets if someone sneezes near the router.

__Cortex__ is here to fix that. Stop fighting with your sockets and let us handle the traffic.

```
1. Install cortex (based on your OS) on the latest cortex release.
```
```
2. For server, 
WINDOWS
cortex.exe server
Linux / macOS
./cortex server 
```
```
3. For client,
WINDOWS
cortex.exe client --connect http://<Your Server IPv4 Address>:50051
Linux / macOS
./cortex.exe client --connect http://<Your Server IPv4 Address>:50051
```
```
4. The TUI will guide you do the rest, just remember that:
commands for server:
- msg <ClientID> <message>
- bc <message>
commands for client:
- upload <Filename>
- download <Filename>
- msg <message>
```

## Contributing
Is your brain wrinklier than ours? Do you want to help Cortex think faster?
```
1. Fork it.
2. Create your feature branch (git checkout -b feature/<Feature Name>).
3. Commit your changes (git commit -m '<Commit Message>').
4. Push to the branch (git push origin feature/<Feature Name>).
5. Open a Pull Request.
```
> __Note__: Please ensure all tests pass. We don't want a lobotomy by accident.
___
> Licensed under __GNU AFFERO GENERAL PUBLIC LICENSE__  

> Maintained by __JuanSign__. May contain traces of caffeine, nicotine, and sarcasm.