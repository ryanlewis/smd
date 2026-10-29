```mermaid
flowchart LR
    S[Sensors] --> M[Microcontroller]
    M -->|MQTT| B[(Broker)]
    B --> D[Database]
    D --> G[Graphs]
```
