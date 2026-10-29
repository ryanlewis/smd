# 🧰 Sensors

Back to [[Weather station]].

| Sensor | Measures | Interface |
| --- | --- | --- |
| BME280 | Temperature, humidity, pressure | I²C |
| Cup anemometer | Wind speed | Pulse counter |
| Wind vane | Wind direction | Analogue |
| Tipping bucket | Rain | Pulse counter |

Each sensor is read once a minute, and the station sends the average every five minutes.

> [!note]
> Keep the BME280 in a ventilated shield, out of direct sun, or it reads several degrees high.

See [[Calibration]] for the offsets applied to each one.
