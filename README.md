# Simulated EMS

Here we have a simulation of an EMS. We have three sites and one cloud.

We will send the events from the gateways to cloud.

The code is in Rust but I'm not an expert this week, maybe I'll refactor it next week. 😉

The code is hosted in Kubernetes, technically, the gateway pods should not have existed as they are installed in local devices. but we have pods for them here.

We also have pods for Postgres (keeps data) and Mosquito (transmits events over MQTT)

We have three crates:
- cloud 
- gws (short for gateway simulation)
- ems-core (common functionalities used by the gateways and the cloud)
