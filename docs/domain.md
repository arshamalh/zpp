# Domain guide: what is the concern behind peakpilot?

Peakpilot is a simulated EMS (energy management system). It's users are usually energy provider businesses and they use this software to have a clear overview of their users and their supply and demand cycles, which leads to managed peak shaving and saving their profit.

As an example, in netherlands, if your usage pass a certain amount once in a month, the whole month energy price will calculate based on that peak!

Netherlands had the problem of grid congestion in the recent years, there are businesses that are queued for getting more electricity for years! the supply chain goes its way, but businesses can optimize their process for quicker access to more affordable energy. We are here to help with the optimization part.

There are three main contributors to this grid congestion story:
- By the raise of AI, EV (electrical vehicles), and HP (Heat Pumps), the need for energy just skyrocket.
- Russia and Iran war, cut two main supply chains of cheap energy.
- Solar panels on the other hand, are the new source of energy, but they should be integrated in to the grid `carefully`.

## Electricity (Energy) Reminder
| Unit | Name | Explanation |
| --- | --- | --- |
| kW | Power | |
| kWh | Energy* | The amount of energy generated, consumed or stored in a battery |
| kWp | kilowatt-peak | The peak of a solar system output under lab situation, which in real-world will be usually lower |

Energy = Power * time => 100 kW for 15 minutes (1/4 of an hour) = 25 kWh.
With 25 kWh, you can ride a Tesla Model 3 for 165 km, or if your 10 Watt lamp be on for 10 hours per day, you can light it for 2500 * 60 / 10 / 10 = 1500 days. 

## The PTU (ISP): 15 minutes
PTU's (by netherlands, which are called ISPs elsewhere) are the 15 minutes time-span for planning, pricing, and reporting. By that, we'll have 96 PTU's per day. each PTU can present the average electricity usage of that time period.

For simulation purposes, in our platform, each PTU passes in one second.


