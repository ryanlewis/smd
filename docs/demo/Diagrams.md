# 🗺️ Diagrams

The [[Weather station]] drawn as one diagram of each common mermaid type, to see how smd draws them.

## Flowchart

```mermaid
flowchart TB
    subgraph Fence
        S[BME280] --> M[Microcontroller]
        A[Anemometer] --> M
        V[Wind vane] --> M
        R[Rain gauge] --> M
    end
    subgraph Shed
        B[(Broker)] --> D[(Database)]
        D --> G[Graphs]
        D --> AL{Frost?}
        AL -->|yes| P[Phone alert]
        AL -->|no| N[Nothing]
    end
    M -->|MQTT over Wi-Fi| B
```

## Sequence

```mermaid
sequenceDiagram
    participant M as Microcontroller
    participant B as Broker
    participant D as Database
    loop Every minute
        M->>M: Read each sensor
    end
    M->>B: Publish the five-minute average
    B->>D: Store the reading
    D-->>B: Stored
    Note over M,B: Readings wait on the microcontroller when Wi-Fi drops
    alt Wi-Fi is down
        M->>M: Keep up to a day of readings
    else Wi-Fi is back
        M->>B: Send what was kept
    end
```

## State

```mermaid
stateDiagram-v2
    [*] --> Sleeping
    Sleeping --> Reading: timer fires
    Reading --> Sending: five readings taken
    Reading --> Sleeping: fewer than five
    Sending --> Sleeping: sent
    Sending --> Keeping: no Wi-Fi
    Keeping --> Sending: Wi-Fi back
    Keeping --> [*]: battery flat
```

## Class

```mermaid
classDiagram
    class Sensor {
        +String name
        +read() Reading
    }
    class Reading {
        +DateTime at
        +f64 value
        +String unit
    }
    class Bme280 {
        +read_pressure() f64
    }
    class Anemometer {
        +u32 pulses
    }
    Sensor <|-- Bme280
    Sensor <|-- Anemometer
    Sensor "1" --> "*" Reading : takes
```

## Entity relationship

```mermaid
erDiagram
    STATION ||--o{ SENSOR : has
    SENSOR ||--o{ READING : takes
    SENSOR ||--o| OFFSET : "corrected by"
    STATION {
        string name
        float latitude
        float longitude
    }
    READING {
        datetime at
        float value
    }
```

## Gantt

```mermaid
gantt
    title Winter jobs
    dateFormat YYYY-MM-DD
    axisFormat %d %b
    tickInterval 2day
    section Hardware
        Rain gauge heater     :a1, 2026-11-02, 5d
        New vane bearing      :a2, after a1, 2d
    section Software
        Frost alert           :b1, 2026-11-02, 3d
        Check against the reference station :b2, after b1, 4d
```

## Pie

```mermaid
pie title Rain last month by day of the week (mm)
    "Monday" : 4.2
    "Tuesday" : 11.6
    "Wednesday" : 7.3
    "Thursday" : 1.1
    "Friday" : 3.8
    "Saturday" : 9.4
    "Sunday" : 2.0
```

## Mindmap

```mermaid
mindmap
  root((Weather station))
    Sensors
      BME280
      Anemometer
      Wind vane
      Rain gauge
    Data
      Broker
      Database
      Graphs
    Jobs
      Heater
      Frost alert
```

## Timeline

```mermaid
timeline
    title The station so far
    2025 : First BME280 on the windowsill
    2026 Spring : Moved to the fence : Anemometer and vane added
    2026 Summer : Rain gauge : Graphs on the shed screen
    2026 Autumn : Calibrated against the reference station
```

## Git graph

```mermaid
gitGraph
    commit id: "read BME280"
    commit id: "send over MQTT"
    branch frost-alert
    commit id: "add threshold"
    commit id: "send phone alert"
    checkout main
    commit id: "fix vane offset"
    merge frost-alert
    commit id: "tag v1.0"
```

## Quadrant

```mermaid
quadrantChart
    title Jobs by effort and value
    x-axis Low effort --> High effort
    y-axis Low value --> High value
    quadrant-1 Plan
    quadrant-2 Do now
    quadrant-3 Maybe
    quadrant-4 Skip
    Frost alert: [0.3, 0.8]
    Rain gauge heater: [0.7, 0.75]
    New vane bearing: [0.2, 0.4]
    Solar panel: [0.85, 0.3]
```

## XY chart

```mermaid
xychart-beta
    title "Last week"
    x-axis [Mon, Tue, Wed, Thu, Fri]
    y-axis "Temperature (°C)" 0 --> 20
    bar [17.2, 15.8, 14.1, 16.5, 18.3]
    line [9.1, 10.4, 8.7, 7.9, 11.2]
```
